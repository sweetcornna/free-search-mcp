"""The stdio smoke client must not launch a configured HTTP server."""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

import pytest

SMOKE_SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "smoke_mcp.py"


@pytest.mark.parametrize("era", ["2026-07-28", "legacy"])
async def test_smoke_launch_explicitly_selects_stdio(monkeypatch, era):
    spec = importlib.util.spec_from_file_location("smoke_mcp", SMOKE_SCRIPT)
    smoke = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(smoke)
    launches = []

    class LaunchCaptured(Exception):
        pass

    def capture_client(server, **kwargs):
        launches.append(server)
        # Stop before opening a subprocess or calling any live tools.
        raise LaunchCaptured

    monkeypatch.setattr(smoke, "Client", capture_client)
    with pytest.raises(LaunchCaptured):
        await smoke._run(era)

    (server,) = launches
    assert server.command == sys.executable
    assert server.args == ["-m", "search_mcp", "--transport", "stdio"]
