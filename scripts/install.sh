#!/usr/bin/env bash
# One-command setup for free-search-mcp.
#
# Local checkout:
#   ./scripts/install.sh
#
# One-line remote install:
#   curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh \
#     | bash -s -- --client claude-code
set -euo pipefail

DEFAULT_REPO_URL="https://github.com/sweetcornna/free-search-mcp.git"
DEFAULT_INSTALL_DIR="$HOME/.local/share/free-search-mcp"

REPO_URL="${SEARCH_MCP_REPO_URL:-$DEFAULT_REPO_URL}"
INSTALL_DIR="${SEARCH_MCP_INSTALL_DIR:-$DEFAULT_INSTALL_DIR}"
CLIENT="none"
SCOPE="user"
DRY_RUN=0
SKIP_BROWSER=0
FORWARD_ARGS=()

bold() { printf '\033[1m%s\033[0m\n' "$1"; }
ok() { printf '\033[32m✓\033[0m %s\n' "$1"; }
warn() { printf '\033[33m!\033[0m %s\n' "$1"; }
die() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: install.sh [options]

Install free-search-mcp dependencies, browser runtime, smoke-test the server,
and optionally register it with an MCP client.

One-line Claude Code install:
  curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh | bash -s -- --client claude-code

Local checkout:
  ./scripts/install.sh

Options:
  --client none             Install only; do not edit any MCP client config (default)
  --client claude-code      Register with Claude Code via `claude mcp add`
  --client claude-desktop   Write Claude Desktop's claude_desktop_config.json
  --client codex            Register with Codex via `codex mcp add`
  --client generic          Print a portable stdio MCP JSON config for other agents
  --client add-mcp          Use `npx add-mcp` to write supported agent configs
  --client both             Register both Claude Code and Claude Desktop
  --client all              Register Claude Code, Claude Desktop, and Codex
  --scope user              Claude Code scope: local, user, or project (default: user)
  --install-dir PATH        Target used by the one-line remote installer
  --repo-url URL            Git repository used by the one-line remote installer
  --skip-browser            Skip `playwright install chromium`
  --dry-run                 Print actions without executing them
  -h, --help                Show this help

Environment:
  SEARCH_MCP_INSTALL_DIR    Same as --install-dir
  SEARCH_MCP_REPO_URL       Same as --repo-url
EOF
}

shell_join() {
  local out="" arg
  for arg in "$@"; do
    if [ -z "$out" ]; then
      printf -v out '%q' "$arg"
    else
      printf -v out '%s %q' "$out" "$arg"
    fi
  done
  printf '%s' "$out"
}

run() {
  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'DRY RUN: %s\n' "$(shell_join "$@")"
  else
    "$@"
  fi
}

run_shell() {
  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'DRY RUN: %s\n' "$1"
  else
    sh -c "$1"
  fi
}

quote_dq() {
  printf '"%s"' "${1//\"/\\\"}"
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      -h|--help)
        usage
        exit 0
        ;;
      --dry-run)
        DRY_RUN=1
        FORWARD_ARGS+=("$1")
        shift
        ;;
      --skip-browser)
        SKIP_BROWSER=1
        FORWARD_ARGS+=("$1")
        shift
        ;;
      --client)
        [ "$#" -ge 2 ] || die "--client requires a value"
        CLIENT="$2"
        FORWARD_ARGS+=("$1" "$2")
        shift 2
        ;;
      --scope)
        [ "$#" -ge 2 ] || die "--scope requires a value"
        SCOPE="$2"
        FORWARD_ARGS+=("$1" "$2")
        shift 2
        ;;
      --install-dir)
        [ "$#" -ge 2 ] || die "--install-dir requires a value"
        INSTALL_DIR="$2"
        shift 2
        ;;
      --repo-url)
        [ "$#" -ge 2 ] || die "--repo-url requires a value"
        REPO_URL="$2"
        shift 2
        ;;
      *)
        die "unknown option: $1"
        ;;
    esac
  done

  case "$CLIENT" in
    none|claude-code|claude-desktop|codex|generic|add-mcp|both|all) ;;
    *) die "--client must be one of: none, claude-code, claude-desktop, codex, generic, add-mcp, both, all" ;;
  esac

  case "$SCOPE" in
    local|user|project) ;;
    *) die "--scope must be one of: local, user, project" ;;
  esac
}

repo_root_from_script() {
  local script_path script_dir root
  script_path="${BASH_SOURCE[0]:-$0}"
  script_dir="$(cd "$(dirname "$script_path")" 2>/dev/null && pwd -P || true)"
  [ -n "$script_dir" ] || return 1
  root="$(cd "$script_dir/.." 2>/dev/null && pwd -P || true)"
  [ -f "$root/pyproject.toml" ] || return 1
  [ -d "$root/src/search_mcp" ] || return 1
  printf '%s' "$root"
}

bootstrap_remote() {
  bold "free-search-mcp · remote installer"
  echo "Repository: $REPO_URL"
  echo "Install dir: $INSTALL_DIR"
  echo

  if [ "$DRY_RUN" -eq 1 ]; then
    if [ -d "$INSTALL_DIR/.git" ]; then
      printf 'DRY RUN: git -C %s pull --ff-only\n' "$(quote_dq "$INSTALL_DIR")"
    else
      printf 'DRY RUN: git clone %s %s\n' "$(quote_dq "$REPO_URL")" "$(quote_dq "$INSTALL_DIR")"
    fi
    printf 'DRY RUN: Re-run local installer: %s %s\n' \
      "$(quote_dq "$INSTALL_DIR/scripts/install.sh")" "$(shell_join "${FORWARD_ARGS[@]}")"
    return 0
  fi

  command -v git >/dev/null 2>&1 || die "git is required for one-line install"
  if [ -d "$INSTALL_DIR/.git" ]; then
    run git -C "$INSTALL_DIR" pull --ff-only
  elif [ -e "$INSTALL_DIR" ]; then
    die "$INSTALL_DIR exists but is not a git checkout; set SEARCH_MCP_INSTALL_DIR to another path"
  else
    run git clone "$REPO_URL" "$INSTALL_DIR"
  fi

  exec "$INSTALL_DIR/scripts/install.sh" "${FORWARD_ARGS[@]}"
}

ensure_uv() {
  if ! command -v uv >/dev/null 2>&1; then
    echo "uv (Python package manager) not found. Installing it..."
    run_shell 'curl -LsSf https://astral.sh/uv/install.sh | sh'
    export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
  fi

  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'DRY RUN: uv --version\n'
  else
    ok "uv $(uv --version | awk '{print $2}')"
  fi
}

desktop_config_path() {
  case "$(uname -s 2>/dev/null || printf unknown)" in
    Darwin)
      printf '%s/Library/Application Support/Claude/claude_desktop_config.json' "$HOME"
      ;;
    Linux)
      printf '%s/.config/Claude/claude_desktop_config.json' "$HOME"
      ;;
    MINGW*|MSYS*|CYGWIN*)
      if [ -n "${APPDATA:-}" ]; then
        printf '%s/Claude/claude_desktop_config.json' "$APPDATA"
      else
        printf '%s/AppData/Roaming/Claude/claude_desktop_config.json' "$HOME"
      fi
      ;;
    *)
      printf '%s/.config/Claude/claude_desktop_config.json' "$HOME"
      ;;
  esac
}

register_claude_code() {
  local cmd
  cmd="claude mcp add search -s $SCOPE -- uv --directory $(quote_dq "$ROOT") run search-mcp"

  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'DRY RUN: %s\n' "$cmd"
    return 0
  fi

  if ! command -v claude >/dev/null 2>&1; then
    warn "Claude Code CLI not found; skipping Claude Code registration."
    printf 'Run manually after installing Claude Code:\n  %s\n' "$cmd"
    return 0
  fi

  if claude mcp add search -s "$SCOPE" -- uv --directory "$ROOT" run search-mcp; then
    ok "registered Claude Code MCP server (scope: $SCOPE)"
  else
    warn "Claude Code registration failed. If an old 'search' server exists, remove it first:"
    printf '  claude mcp remove search -s %s\n' "$SCOPE"
    return 1
  fi
}

register_claude_desktop() {
  local config_path
  config_path="$(desktop_config_path)"

  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'DRY RUN: update Claude Desktop config %s with search -> uv --directory %s run search-mcp\n' \
      "$(quote_dq "$config_path")" "$(quote_dq "$ROOT")"
    return 0
  fi

  mkdir -p "$(dirname "$config_path")"
  uv run python - "$config_path" "$ROOT" <<'PY'
import json
import sys
from pathlib import Path

config_path = Path(sys.argv[1])
root = sys.argv[2]

if config_path.exists() and config_path.read_text(encoding="utf-8").strip():
    try:
        data = json.loads(config_path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise SystemExit(f"{config_path} is not valid JSON: {exc}") from exc
else:
    data = {}

servers = data.setdefault("mcpServers", {})
servers["search"] = {
    "command": "uv",
    "args": ["--directory", root, "run", "search-mcp"],
}

tmp = config_path.with_suffix(config_path.suffix + ".tmp")
tmp.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
tmp.replace(config_path)
PY
  ok "updated Claude Desktop config: $config_path"
  warn "Restart Claude Desktop to load the search MCP server."
}

register_codex() {
  local cmd
  cmd="codex mcp add search -- uv --directory $(quote_dq "$ROOT") run search-mcp"

  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'DRY RUN: %s\n' "$cmd"
    return 0
  fi

  if ! command -v codex >/dev/null 2>&1; then
    warn "Codex CLI not found; skipping Codex registration."
    printf 'Run manually after installing Codex:\n  %s\n' "$cmd"
    return 0
  fi

  if codex mcp add search -- uv --directory "$ROOT" run search-mcp; then
    ok "registered Codex MCP server"
  else
    warn "Codex registration failed. If an old 'search' server exists, remove it first:"
    printf '  codex mcp remove search\n'
    return 1
  fi
}

print_generic_agent_config() {
  cat <<EOF

Portable stdio MCP config for other agents:

{
  "mcpServers": {
    "search": {
      "command": "uv",
      "args": ["--directory", "$ROOT", "run", "search-mcp"]
    }
  }
}

Agent usage guide: $ROOT/docs/AGENT_USAGE.md
EOF
}

register_add_mcp() {
  local server_cmd add_cmd
  server_cmd="uv --directory $(quote_dq "$ROOT") run search-mcp"
  add_cmd="npx -y add-mcp $(quote_dq "$server_cmd") --name search --all -g -y"

  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'DRY RUN: %s\n' "$add_cmd"
    return 0
  fi

  if ! command -v npx >/dev/null 2>&1; then
    warn "npx not found; cannot run add-mcp."
    printf 'Run this after installing Node.js/npm:\n  %s\n' "$add_cmd"
    return 0
  fi

  if npx -y add-mcp "$server_cmd" --name search --all -g -y; then
    ok "ran add-mcp for supported global agent configs"
  else
    warn "add-mcp failed. You can still copy the generic config below."
    print_generic_agent_config
    return 1
  fi
}

register_clients() {
  case "$CLIENT" in
    none)
      return 0
      ;;
    claude-code)
      register_claude_code
      ;;
    claude-desktop)
      register_claude_desktop
      ;;
    codex)
      register_codex
      ;;
    generic)
      print_generic_agent_config
      ;;
    add-mcp)
      register_add_mcp
      ;;
    both)
      register_claude_code
      register_claude_desktop
      ;;
    all)
      register_claude_code
      register_claude_desktop
      register_codex
      ;;
  esac
}

print_next_steps() {
  echo
  bold "Done."

  if [ "$CLIENT" = "none" ]; then
    cat <<EOF

Wire it into a client:

  Claude Code:
    claude mcp add search -s user -- uv --directory "$ROOT" run search-mcp

  Claude Desktop:
    Add this to claude_desktop_config.json:
      {
        "mcpServers": {
          "search": {
            "command": "uv",
            "args": ["--directory", "$ROOT", "run", "search-mcp"]
          }
        }
      }

  Run standalone (stdio):
    uv --directory "$ROOT" run search-mcp
EOF
  else
    cat <<EOF

Registered client option: $CLIENT

Run standalone (stdio):
  uv --directory "$ROOT" run search-mcp

Other agents:
  See "$ROOT/docs/AGENT_USAGE.md" for Codex, Cursor, Cline, Continue, Zed, and generic MCP config.
EOF
  fi
}

parse_args "$@"

ROOT="$(repo_root_from_script || true)"
if [ -z "$ROOT" ]; then
  bootstrap_remote
  exit $?
fi

cd "$ROOT"

bold "free-search-mcp · one-click setup"
echo "Project: $ROOT"
echo

ensure_uv

echo "Syncing dependencies (uv sync)..."
run uv sync
[ "$DRY_RUN" -eq 1 ] || ok "dependencies installed"

if [ "$SKIP_BROWSER" -eq 0 ]; then
  echo "Installing the Chromium browser for Playwright..."
  run uv run playwright install chromium
  [ "$DRY_RUN" -eq 1 ] || ok "chromium installed"
else
  warn "skipping Chromium install"
fi

if [ ! -f .env ] && [ -f .env.example ]; then
  run cp .env.example .env
  [ "$DRY_RUN" -eq 1 ] || ok "created .env from .env.example (edit to customize)"
fi

if [ "$DRY_RUN" -eq 0 ]; then
  echo "Smoke-testing the engine registry..."
fi
run uv run python -c "from search_mcp.aggregator import list_engines; print('engines:', ', '.join(list_engines()))"
[ "$DRY_RUN" -eq 1 ] || ok "server imports cleanly"

register_clients
print_next_steps
