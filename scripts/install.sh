#!/usr/bin/env bash
# One-command setup for free-search-mcp.
#   ./scripts/install.sh
# Installs Python deps + the Chromium browser, then prints how to wire the
# server into Claude Code / Claude Desktop.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

bold() { printf '\033[1m%s\033[0m\n' "$1"; }
ok()   { printf '\033[32m✓\033[0m %s\n' "$1"; }

bold "free-search-mcp · one-click setup"
echo "Project: $ROOT"
echo

# 1. uv ----------------------------------------------------------------------
if ! command -v uv >/dev/null 2>&1; then
  echo "uv (Python package manager) not found. Installing it..."
  curl -LsSf https://astral.sh/uv/install.sh | sh
  # shellcheck disable=SC1090
  export PATH="$HOME/.local/bin:$PATH"
fi
ok "uv $(uv --version | awk '{print $2}')"

# 2. dependencies ------------------------------------------------------------
echo "Syncing dependencies (uv sync)..."
uv sync
ok "dependencies installed"

# 3. browser (for the browser-rendered engines: startpage/bing/zhihu + fetch) -
echo "Installing the Chromium browser for Playwright..."
uv run playwright install chromium
ok "chromium installed"

# 4. optional .env -----------------------------------------------------------
if [ ! -f .env ] && [ -f .env.example ]; then
  cp .env.example .env
  ok "created .env from .env.example (edit to customize)"
fi

# 5. smoke test --------------------------------------------------------------
echo "Smoke-testing the engine registry..."
uv run python -c "from search_mcp.aggregator import list_engines; print('engines:', ', '.join(list_engines()))"
ok "server imports cleanly"

echo
bold "Done. Wire it into a client:"
cat <<EOF

  Claude Code (this repo is already configured via .mcp.json):
    Just run 'claude' inside $ROOT — it auto-detects the 'search' server.
    Or register it globally:
      claude mcp add search uv -- --directory "$ROOT" run search-mcp

  Claude Desktop — add to claude_desktop_config.json:
    {
      "mcpServers": {
        "search": {
          "command": "uv",
          "args": ["--directory", "$ROOT", "run", "search-mcp"]
        }
      }
    }

  Run standalone (stdio):
    uv run search-mcp
EOF
