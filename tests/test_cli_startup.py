"""Help and usage errors must work before runtime configuration is loaded."""
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

import pytest


@pytest.fixture
def cli(tmp_path):
    home = tmp_path / "home"
    cwd = tmp_path / "cwd"
    config = tmp_path / "config"
    for directory in (home, cwd, config):
        directory.mkdir()
    # Do not inherit a developer's credentials, backend, or editable source path.
    env = {
        key: value for key, value in os.environ.items()
        if key.upper() in {"SYSTEMROOT", "WINDIR", "PATH", "PATHEXT", "COMSPEC"}
    }
    env.update({
        "HOME": str(home),
        "USERPROFILE": str(home),
        "APPDATA": str(home / "AppData" / "Roaming"),
        "LOCALAPPDATA": str(home / "AppData" / "Local"),
        "XDG_CONFIG_HOME": str(home / ".config"),
        "XDG_CACHE_HOME": str(home / ".cache"),
        "CODEX_HOME": str(home / ".codex"),
        "TEMP": str(tmp_path),
        "TMP": str(tmp_path),
        "PYTHONPATH": str(Path(__file__).resolve().parents[1] / "src"),
        "PYTHONDONTWRITEBYTECODE": "1",
        "PYTHONUTF8": "1",
        "SEARCH_MCP_CONFIG_DIR": str(config),
        "SEARCH_MCP_CACHE_DIR": str(tmp_path / "cache"),
        "SEARCH_MCP_AGENT_BACKEND": "off",
        "SEARCH_MCP_PREWARM": "false",
        "SEARCH_MCP_CODEX_AUTO_SIGNIN": "false",
    })

    def run(args):
        return subprocess.run(
            [sys.executable, "-m", "search_mcp", *args],
            cwd=cwd, env=env, capture_output=True, text=True, encoding="utf-8", timeout=30,
        )

    return run, env, cwd, config


@pytest.mark.parametrize("configuration", ["default", "invalid-transport", "invalid-dotenv"])
@pytest.mark.parametrize(
    "args, exit_code, message",
    [
        (["--help"], 0, "--transport"),
        (["ask", "--help"], 0, "--freshness"),
        (["agent-file", "--help"], 0, "--tool-prefix"),
        (["--port", "not-a-number"], 2, "invalid int value"),
        (["ask"], 2, "question"),
        (["agent-file", "unknown-host"], 2, "invalid choice"),
    ],
)
def test_help_and_usage_errors_do_not_initialize_runtime(
    cli, configuration, args, exit_code, message,
):
    run, env, cwd, _ = cli
    if configuration == "invalid-transport":
        env["SEARCH_MCP_TRANSPORT"] = "invalid"
    elif configuration == "invalid-dotenv":
        (cwd / ".env").write_bytes(b"\xff")

    result = run(args)

    assert result.returncode == exit_code, result.stderr
    shown = result.stdout if exit_code == 0 else result.stderr
    assert "usage:" in shown
    assert message in shown
    assert "Traceback" not in result.stderr
    assert not Path(env["SEARCH_MCP_CACHE_DIR"]).exists()


@pytest.mark.parametrize(
    "options, tools",
    [
        (
            [],
            "mcp__search__research, mcp__search__search, "
            "mcp__search__fetch, mcp__search__read_doc",
        ),
        (["--tool-prefix", ""], "research, search, fetch, read_doc"),
        (
            ["--tool-prefix", "mcp__websearch__"],
            "mcp__websearch__research, mcp__websearch__search, "
            "mcp__websearch__fetch, mcp__websearch__read_doc",
        ),
    ],
)
def test_agent_file_preserves_omitted_empty_and_custom_tool_prefix(cli, options, tools):
    run, _, _, _ = cli

    result = run(["agent-file", "claude-code", *options])

    assert result.returncode == 0, result.stderr
    assert f"tools: {tools}" in result.stdout.splitlines()
