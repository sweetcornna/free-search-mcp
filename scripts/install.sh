#!/usr/bin/env bash
# One-command setup for free-search-mcp 1.0 (Rust binary + `search-mcp` alias).
#
# Most people want the tri-client plugin instead (pinned version, skills + agent, no API key):
#   Claude Code:  /plugin marketplace add sweetcornna/free-search-mcp
#                 /plugin install free-search@free-search-mcp
#   Codex:        codex plugin marketplace add sweetcornna/free-search-mcp
#                 codex plugin add free-search@free-search-mcp
#   Antigravity:  agy plugin install https://github.com/sweetcornna/free-search-mcp/tree/main/plugins/free-search
#
# Local checkout (builds release binary, installs ~/.local/bin/free-search-mcp + search-mcp alias):
#   ./scripts/install.sh
#
# One-line remote install:
#   curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh \
#     | bash -s -- --client claude-code
set -euo pipefail

DEFAULT_REPO_URL="https://github.com/sweetcornna/free-search-mcp.git"
DEFAULT_INSTALL_DIR="$HOME/.local/share/free-search-mcp"
DEFAULT_BIN_DIR="$HOME/.local/bin"

REPO_URL="${SEARCH_MCP_REPO_URL:-$DEFAULT_REPO_URL}"
INSTALL_DIR="${SEARCH_MCP_INSTALL_DIR:-$DEFAULT_INSTALL_DIR}"
BIN_DIR="${SEARCH_MCP_BIN_DIR:-$DEFAULT_BIN_DIR}"
CLIENT="none"
SCOPE="user"
DRY_RUN=0
SKIP_SMOKE=0
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

Build and install the free-search-mcp 1.0 Rust binary into ~/.local/bin
(with a `search-mcp` symlink alias), run a quick verification check, and
optionally register with an MCP client. No API key is needed, now or later.

Prefer the plugin (pinned version, verified-research + scholar-research skills,
quick-search agent, no build required):
  Claude Code:  /plugin marketplace add sweetcornna/free-search-mcp
                /plugin install free-search@free-search-mcp
  Codex:        codex plugin marketplace add sweetcornna/free-search-mcp
                codex plugin add free-search@free-search-mcp
  Antigravity:  agy plugin install https://github.com/sweetcornna/free-search-mcp/tree/main/plugins/free-search

One-line source install:
  curl -LsSf https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/scripts/install.sh | bash -s -- --client claude-code

Local checkout:
  ./scripts/install.sh

Options:
  --client none             Install binary only; do not edit any client config (default)
  --client claude-code      Register with Claude Code via `claude mcp add`
  --client claude-desktop   Write Claude Desktop's claude_desktop_config.json
  --client codex            Register with Codex via `codex mcp add`
  --client antigravity      Register with Antigravity (`agy`) via `agy mcp add`
  --client generic          Print a portable stdio MCP JSON config for other agents
  --client add-mcp          Use `npx add-mcp` to write supported agent configs
  --client both             Register Claude Code and Claude Desktop
  --client all              Register Claude Code, Claude Desktop, Codex, and Antigravity
  --scope user              Claude Code scope: local, user, or project (default: user)
  --bin-dir PATH            Binary install directory (default: ~/.local/bin)
  --install-dir PATH        Checkout path used by the one-line remote installer
  --repo-url URL            Git repository used by the one-line remote installer
  --skip-smoke              Skip `free-search-mcp --help` check after install
  --dry-run                 Print actions without executing them
  -h, --help                Show this help

Environment:
  SEARCH_MCP_BIN_DIR        Same as --bin-dir
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
      --skip-smoke|--skip-browser)
        SKIP_SMOKE=1
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
      --bin-dir)
        [ "$#" -ge 2 ] || die "--bin-dir requires a value"
        BIN_DIR="$2"
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
    none|claude-code|claude-desktop|codex|antigravity|generic|add-mcp|both|all) ;;
    *) die "--client must be one of: none, claude-code, claude-desktop, codex, antigravity, generic, add-mcp, both, all" ;;
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
  [ -f "$root/Cargo.toml" ] || return 1
  [ -f "$root/crates/server/Cargo.toml" ] || return 1
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

  command -v git >/dev/null 2>&1 || die "git is required for one-line source install"
  if [ -d "$INSTALL_DIR/.git" ]; then
    run git -C "$INSTALL_DIR" pull --ff-only
  elif [ -e "$INSTALL_DIR" ]; then
    die "$INSTALL_DIR exists but is not a git checkout; set SEARCH_MCP_INSTALL_DIR to another path"
  else
    run git clone "$REPO_URL" "$INSTALL_DIR"
  fi

  exec "$INSTALL_DIR/scripts/install.sh" "${FORWARD_ARGS[@]}"
}

ensure_cargo() {
  if ! command -v cargo >/dev/null 2>&1; then
    die "cargo not found. Install Rust (https://rustup.rs) or install the prebuilt wheel via \`uvx free-search-mcp\`."
  fi
  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'DRY RUN: cargo --version\n'
  else
    ok "$(cargo --version)"
  fi
}

install_binary() {
  local src_bin="$ROOT/target/release/free-search-mcp"
  local dst_bin="$BIN_DIR/free-search-mcp"
  local alias_bin="$BIN_DIR/search-mcp"
  local tmp_bin="$BIN_DIR/.free-search-mcp.tmp.$$"

  if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    src_bin="${CARGO_TARGET_DIR}/release/free-search-mcp"
  fi

  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'DRY RUN: mkdir -p %s\n' "$(quote_dq "$BIN_DIR")"
    printf 'DRY RUN: install -m 755 %s %s && mv -f %s %s (replaces symlink safely)\n' \
      "$(quote_dq "$src_bin")" "$(quote_dq "$tmp_bin")" "$(quote_dq "$tmp_bin")" "$(quote_dq "$dst_bin")"
    printf 'DRY RUN: ln -sfn free-search-mcp %s\n' "$(quote_dq "$alias_bin")"
    return 0
  fi

  [ -x "$src_bin" ] || die "expected built binary at $src_bin"
  mkdir -p "$BIN_DIR"
  # Copy to a sibling temp file and atomically mv so an existing symlink
  # (e.g. from an earlier `uv tool install`) is replaced itself rather than
  # written through into uv's tool directory.
  install -m 755 "$src_bin" "$tmp_bin"
  mv -f "$tmp_bin" "$dst_bin"
  ln -sfn free-search-mcp "$alias_bin"
  ok "installed $dst_bin and alias $alias_bin -> free-search-mcp"
}

desktop_config_path() {
  case "$(uname -s 2>/dev/null || printf unknown)" in
    Darwin)
      printf '%s/Library/Application Support/Claude/claude_desktop_config.json' "$HOME"
      ;;
    Linux)
      printf '%s/.config/Claude/claude_desktop_config.json' "$HOME"
      ;;
    *)
      printf '%s/.config/Claude/claude_desktop_config.json' "$HOME"
      ;;
  esac
}

register_claude_code() {
  local bin="$BIN_DIR/free-search-mcp"
  local cmd="claude mcp add search -s $SCOPE -- $(quote_dq "$bin")"

  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'DRY RUN: %s\n' "$cmd"
    return 0
  fi

  if ! command -v claude >/dev/null 2>&1; then
    warn "Claude Code CLI not found; skipping Claude Code registration."
    printf 'Run manually after installing Claude Code:\n  %s\n' "$cmd"
    return 0
  fi

  if claude mcp add search -s "$SCOPE" -- "$bin"; then
    ok "registered Claude Code MCP server 'search' (scope: $SCOPE)"
  else
    warn "Claude Code registration failed. If an old 'search' server exists, remove it first:"
    printf '  claude mcp remove search -s %s\n' "$SCOPE"
    return 1
  fi
}

register_claude_desktop() {
  local config_path bin
  config_path="$(desktop_config_path)"
  bin="$BIN_DIR/free-search-mcp"

  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'DRY RUN: update Claude Desktop config %s with search -> %s\n' \
      "$(quote_dq "$config_path")" "$(quote_dq "$bin")"
    return 0
  fi

  command -v python3 >/dev/null 2>&1 || die "python3 is required to update $config_path safely"
  mkdir -p "$(dirname "$config_path")"
  if [ -f "$config_path" ]; then
    local backup="${config_path}.bak.$(date +%Y%m%d%H%M%S)"
    cp "$config_path" "$backup"
    ok "backed up $config_path -> $backup"
  fi
  python3 - "$config_path" "$bin" <<'PY'
import json
import sys
from pathlib import Path

config_path = Path(sys.argv[1])
bin_path = sys.argv[2]

if config_path.exists() and config_path.read_text(encoding="utf-8").strip():
    try:
        data = json.loads(config_path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise SystemExit(f"{config_path} is not valid JSON: {exc}") from exc
else:
    data = {}

servers = data.setdefault("mcpServers", {})
servers["search"] = {
    "command": bin_path,
    "args": [],
}

tmp = config_path.with_suffix(config_path.suffix + ".tmp")
tmp.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
tmp.replace(config_path)
PY
  ok "updated Claude Desktop config: $config_path"
  warn "Restart Claude Desktop to load the search MCP server."
}

register_codex() {
  local bin="$BIN_DIR/free-search-mcp"
  local cmd="codex mcp add search -- $(quote_dq "$bin")"

  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'DRY RUN: %s\n' "$cmd"
    return 0
  fi

  if ! command -v codex >/dev/null 2>&1; then
    warn "Codex CLI not found; skipping Codex registration."
    printf 'Run manually after installing Codex:\n  %s\n' "$cmd"
    return 0
  fi

  if codex mcp add search -- "$bin"; then
    ok "registered Codex MCP server 'search'"
    warn "Tip: add \`tool_timeout_sec = 150\` under [mcp_servers.search] in ~/.codex/config.toml so long \`exec\` scripts (up to 120s) are not clipped."
  else
    warn "Codex registration failed. If an old 'search' server exists, remove it first:"
    printf '  codex mcp remove search\n'
    return 1
  fi
}

register_antigravity() {
  local bin="$BIN_DIR/free-search-mcp"
  local cmd="agy mcp add search $(quote_dq "$bin")"

  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'DRY RUN: %s\n' "$cmd"
    return 0
  fi

  if ! command -v agy >/dev/null 2>&1; then
    warn "Antigravity CLI (agy) not found; skipping Antigravity registration."
    printf 'Run manually after installing Antigravity:\n  %s\n' "$cmd"
    return 0
  fi

  if agy mcp add search "$bin"; then
    ok "registered Antigravity (agy) MCP server 'search'"
  else
    warn "Antigravity registration failed. If an old 'search' server exists, remove it first:"
    printf '  agy mcp remove search\n'
    return 1
  fi
}

print_generic_agent_config() {
  local bin="$BIN_DIR/free-search-mcp"
  cat <<EOF

Portable stdio MCP config for other agents:

{
  "mcpServers": {
    "search": {
      "command": "$bin",
      "args": []
    }
  }
}

Agent usage guide: $ROOT/docs/AGENT_USAGE.md
EOF
}

register_add_mcp() {
  local bin="$BIN_DIR/free-search-mcp"
  local add_cmd="npx -y add-mcp $(quote_dq "$bin") --name search --all -g -y"

  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'DRY RUN: %s\n' "$add_cmd"
    return 0
  fi

  if ! command -v npx >/dev/null 2>&1; then
    warn "npx not found; cannot run add-mcp."
    printf 'Run this after installing Node.js/npm:\n  %s\n' "$add_cmd"
    return 0
  fi

  if npx -y add-mcp "$bin" --name search --all -g -y; then
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
    antigravity)
      register_antigravity
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
      register_antigravity
      ;;
  esac
}

print_next_steps() {
  local bin="$BIN_DIR/free-search-mcp"
  echo
  bold "Done."

  if [ "$CLIENT" = "none" ]; then
    cat <<EOF

Wire it into a client (server key "search"):

  Claude Code:
    claude mcp add search -s user -- "$bin"

  Codex:
    codex mcp add search -- "$bin"

  Antigravity (agy):
    agy mcp add search "$bin"

  Claude Desktop / generic MCP JSON:
    {
      "mcpServers": {
        "search": {
          "command": "$bin",
          "args": []
        }
      }
    }

  Run standalone (stdio) or diagnostics:
    free-search-mcp --help
    search-mcp doctor
EOF
  else
    cat <<EOF

Registered client option: $CLIENT

Binary and alias:
  $BIN_DIR/free-search-mcp
  $BIN_DIR/search-mcp -> free-search-mcp

Diagnostics:
  free-search-mcp doctor
  free-search-mcp sources

Other agents:
  See "$ROOT/docs/AGENT_USAGE.md", "$ROOT/docs/CODEX_SEARCH.md", and "$ROOT/docs/ANTIGRAVITY_SEARCH.md".
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

bold "free-search-mcp 1.0 · Rust build & setup"
echo "Project: $ROOT"
echo "Bin dir: $BIN_DIR"
echo

ensure_cargo

echo "Building release binary (cargo build --release --locked -p fsm-server)..."
run cargo build --release --locked -p fsm-server --bin free-search-mcp
[ "$DRY_RUN" -eq 1 ] || ok "release binary built"

install_binary

if [ "$SKIP_SMOKE" -eq 0 ]; then
  if [ "$DRY_RUN" -eq 0 ]; then
    echo "Verifying binary..."
  fi
  run "$BIN_DIR/free-search-mcp" --help >/dev/null
  run "$BIN_DIR/search-mcp" --help >/dev/null
  [ "$DRY_RUN" -eq 1 ] || ok "free-search-mcp and search-mcp respond cleanly"
fi

register_clients
print_next_steps
