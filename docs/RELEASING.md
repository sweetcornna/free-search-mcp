# Releasing

A version of this project is six artifacts, and a release is finished only when
all six agree:

| Where | What it is |
| --- | --- |
| PyPI | the `free-search-mcp` wheel and sdist that every install path downloads |
| GitHub Release | the public record: those two files plus the `.mcpb` bundle |
| CHANGELOG | the release notes, which the workflow copies verbatim |
| Plugin | `plugins/free-search` on `main`, which pins the PyPI version it runs |
| Claude Desktop bundle | `free-search-mcp-X.Y.Z.mcpb`, packed from `mcpb/`. It contains no server code, only a pin on the PyPI version |
| MCP Registry | the `io.github.sweetcornna/free-search-mcp` entry, built from `server.json`, which points at the PyPI package and the bundle |

A tag that reached PyPI without a GitHub Release is an unfinished release. So
is a plugin still pinned to the previous version.

## The version number lives in nine files

| File | Why it has to change |
| --- | --- |
| `pyproject.toml` → `project.version` | the version PyPI publishes |
| `uv.lock` | records the workspace version; the release runs `uv lock --check` |
| `plugins/free-search/.claude-plugin/plugin.json` → `version` | what `/plugin install` reports and what `/plugin update` compares against |
| `plugins/free-search/.mcp.json` → `mcpServers.search.args` | the `free-search-mcp==X.Y.Z` pin the installed plugin runs |
| `plugins/free-search/plugin.json` → `version` | the same plugin as an Agent Plugins v1 package, which `hermes plugins install` reads |
| `plugins/free-search/mcp.json` → `mcpServers.search.args` | the pin that Agent Plugins install runs |
| `server.json` → `version` and `packages[0].version` | what the MCP Registry lists. A published registry version cannot be edited, only superseded |
| `mcpb/manifest.json` → `version` | what Claude Desktop shows for the installed bundle |
| `mcpb/pyproject.toml` → `project.version` and the `free-search-mcp==X.Y.Z` dependency | the pin the installed bundle runs |

Two gates keep the copies together. A forgotten copy fails one of them before
an install can advertise one version and start another.

- `tests/test_plugin_manifest.py` runs in CI on every PR and checks every copy
  against `pyproject.toml`. It also holds the bundle's tool list to the
  server's, holds the tool and parameter names in the skill and the agent
  prompt to the real schemas, and holds the Claude Code and Codex agent files
  to the one prompt in `src/search_mcp/agent.py`.
- The "Validate release metadata" step in `.github/workflows/release.yml`
  checks all of them again, this time against the pushed tag, before anything
  is built or published.

## Checklist

1. Bump the version in the nine files above (`uv lock` regenerates the lock).
   `uv run pytest tests/test_plugin_manifest.py` names any copy you missed.
2. Add a `## [X.Y.Z] - YYYY-MM-DD` section to `CHANGELOG.md`. Everything
   between it and the next `## [` becomes the GitHub Release body, so write it
   for people who will read it as release notes.
3. `uv run ruff check . && uv run pytest -q`.
4. Optional, because CI does not have these tools. The test suite runs the
   first pair by itself when the `claude` CLI is on `PATH`:
   ```bash
   claude plugin validate . --strict && claude plugin validate ./plugins/free-search --strict
   npx --yes @anthropic-ai/mcpb@2.1.2 validate mcpb/manifest.json
   ```
5. Commit to `main` and push.
6. Create an annotated tag. Its subject becomes the GitHub Release title:
   ```bash
   git tag -a vX.Y.Z -m "short release title"
   git push origin vX.Y.Z
   ```
7. The `Release` workflow then runs the full `ci.yml` matrix, including the
   in-range `sdk-canary` leg. It validates the tag against the version, the
   CHANGELOG, the plugin, the registry entry and the bundle. It builds the
   distributions, packs the bundle, and creates a draft GitHub Release with the
   three artifacts. It publishes to PyPI (`skip-existing`, token secret
   `PYPI_API_TOKEN`) and only then takes the release out of draft, marking it
   Latest if no higher stable version exists. A final `registry` job lists the
   version in the MCP Registry.
8. Verify (below) before calling it done.

The workflow is idempotent, so a failed run can be re-run on the same tag. It
reuses an existing draft when the assets match, deletes and rebuilds a draft
when they do not, and refuses to overwrite a published release whose assets
disagree with the tag.

## The MCP Registry step, and what it needs from the tag commit

The `registry` job authenticates with GitHub OIDC, so there is no secret to
configure. The registry grants the `io.github.sweetcornna/*` namespace to
workflows of repositories owned by that account. The job holds
`id-token: write` and has neither repository write access nor the PyPI token.

The registry proves that a PyPI package belongs to a server name by looking for
this comment in the package description, which PyPI builds from `README.md`:

```html
<!-- mcp-name: io.github.sweetcornna/free-search-mcp -->
```

PyPI freezes a release's description at upload, so the marker has to be in the
tagged commit. A marker added afterwards stays invisible to the registry until
the next release. It sits on line 2 of the README, and both gates above check
for it.

The job does four things in order:

1. It skips everything if the registry already lists this version. Registry
   versions are immutable, and this check is what makes a re-run safe.
2. It waits until PyPI's JSON API shows the marker for the new version, which
   can trail the upload by minutes.
3. It downloads the `.mcpb` from the published release and appends it to a
   working copy of `server.json` together with that file's SHA-256. Packing the
   bundle again would give a different file, because archives carry timestamps,
   and no client could match its checksum.
4. It runs `mcp-publisher login github-oidc && mcp-publisher publish`.

As a consequence, `server.json` in the repository lists one package, the PyPI
one. The bundle entry exists only in the published copy.

If this job fails, the release is still complete everywhere else. Re-run it.

## What the plugin release actually is

The plugin needs no release step of its own. There is no second repository, no
marketplace registry to notify and no separate plugin tag.
`/plugin marketplace add sweetcornna/free-search-mcp` clones this repo's default
branch, so the plugin ships the moment the release commit lands on `main`.
Existing users move with `/plugin update free-search` (restart required).

This creates one ordering constraint. Between the release commit landing on
`main` and PyPI accepting the upload, the plugin on `main` pins a version PyPI
does not have yet, and a plugin installed in that window cannot start its
server. So:

- push the tag immediately after the release commit, and do not let a bumped
  `main` sit untagged;
- if the workflow fails, either fix forward quickly or revert the bump commit
  on `main`, so the pin never points at a version that will never exist.

## Verify

```bash
uvx free-search-mcp==X.Y.Z --help          # PyPI has the new version
gh release view vX.Y.Z                     # exists, three assets, marked Latest
curl -s "https://registry.modelcontextprotocol.io/v0/servers/io.github.sweetcornna%2Ffree-search-mcp/versions/X.Y.Z" \
  | jq '.server.packages[] | {registryType, identifier, version}'   # pypi + mcpb
```

The bundle can be checked without Claude Desktop. This is the command its
manifest runs:

```bash
gh release download vX.Y.Z --pattern '*.mcpb' --dir /tmp/bundle
unzip -q /tmp/bundle/*.mcpb -d /tmp/bundle/unpacked
uv run --directory /tmp/bundle/unpacked src/server.py </dev/null   # installs X.Y.Z, exits on EOF
```

To check that the server still answers every tool over stdio, in both protocol
eras:

```bash
uv run python scripts/smoke_mcp.py
```

And the plugin path end to end, in a scratch scope you can throw away:

```bash
claude plugin marketplace add sweetcornna/free-search-mcp
claude plugin install free-search@free-search-mcp -s local
claude plugin details free-search@free-search-mcp   # version, 1 MCP server (search), 1 skill, 1 agent
claude plugin uninstall free-search@free-search-mcp -s local
```

## Notes

- The PyPI distribution name is `free-search-mcp`. The name `search-mcp`
  belongs to another account and uploads there return 403, so do not retry it.
  The import package, the console scripts and the `SEARCH_MCP_*` env vars are
  all unaffected.
- The plugin, the Claude Desktop bundle, the registry entry, the `uvx`
  one-liner, `scripts/install.sh` and Docker are six wrappers around the same
  server. A release changes the version they resolve to. It does not change the
  contract between them.
- `@anthropic-ai/mcpb` and `mcp-publisher` are pinned in `release.yml` (`2.1.2`
  and `v1.8.1`). Bump them on purpose, as with the SDK, so that a release does
  not change because a tool did.
