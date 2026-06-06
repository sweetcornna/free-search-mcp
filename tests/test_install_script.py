import os
import shutil
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "install.sh"


def run_install_script(*args: str, cwd: Path | None = None, env: dict[str, str] | None = None):
    merged_env = os.environ.copy()
    if env:
        merged_env.update(env)
    return subprocess.run(
        ["bash", str(SCRIPT), *args],
        cwd=cwd or ROOT,
        env=merged_env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )


def test_install_script_help_is_side_effect_free():
    result = run_install_script("--help")

    assert result.returncode == 0
    assert "Usage: install.sh" in result.stdout
    assert "curl -LsSf" in result.stdout
    assert "--client claude-code" in result.stdout
    assert "--client codex" in result.stdout
    assert "--client generic" in result.stdout
    assert "--client add-mcp" in result.stdout
    assert "Syncing dependencies" not in result.stdout


def test_install_script_dry_run_prints_claude_code_registration():
    result = run_install_script("--dry-run", "--client", "claude-code", "--scope", "user")

    assert result.returncode == 0
    assert "DRY RUN" in result.stdout
    assert "uv sync" in result.stdout
    assert f'claude mcp add search -s user -- uv --directory "{ROOT}" run search-mcp' in result.stdout
    assert "Smoke-testing the engine registry" not in result.stdout


def test_install_script_dry_run_prints_codex_registration():
    result = run_install_script("--dry-run", "--client", "codex")

    assert result.returncode == 0
    assert f'codex mcp add search -- uv --directory "{ROOT}" run search-mcp' in result.stdout


def test_install_script_dry_run_prints_generic_agent_config():
    result = run_install_script("--dry-run", "--client", "generic")

    assert result.returncode == 0
    assert '"mcpServers"' in result.stdout
    assert '"command": "uv"' in result.stdout
    assert f'"--directory", "{ROOT}", "run", "search-mcp"' in result.stdout
    assert "docs/AGENT_USAGE.md" in result.stdout


def test_piped_installer_dry_run_bootstraps_repo(tmp_path):
    copied_script = tmp_path / "install.sh"
    shutil.copy2(SCRIPT, copied_script)
    install_dir = tmp_path / "free-search-mcp"

    result = subprocess.run(
        ["bash", str(copied_script), "--dry-run", "--client", "none"],
        cwd=tmp_path,
        env={
            **os.environ,
            "SEARCH_MCP_INSTALL_DIR": str(install_dir),
            "SEARCH_MCP_REPO_URL": "https://github.com/sweetcornna/free-search-mcp.git",
        },
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )

    assert result.returncode == 0
    assert "DRY RUN" in result.stdout
    assert f'git clone "https://github.com/sweetcornna/free-search-mcp.git" "{install_dir}"' in result.stdout
    assert "Re-run local installer" in result.stdout


def test_agent_usage_doc_is_linked_and_covers_core_clients():
    doc = ROOT / "docs" / "AGENT_USAGE.md"
    assert doc.exists()

    content = doc.read_text(encoding="utf-8")
    for expected in ["Codex", "Claude Code", "Claude Desktop", "Cursor", "Cline", "Continue", "Zed"]:
        assert expected in content
    assert "uv --directory" in content
    assert "search" in content and "research" in content and "fetch" in content

    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    usage = (ROOT / "docs" / "USAGE.md").read_text(encoding="utf-8")
    assert "docs/AGENT_USAGE.md" in readme
    assert "AGENT_USAGE.md" in usage
