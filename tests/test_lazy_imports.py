"""The server starts without the tools' heavy dependencies.

Measured 2026-10-22, importing them with the server module was 10 s of a
freshly installed server's 12 s start, mostly the first load of native
extensions. These tests keep them out of the import that the handshake waits
for.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys

import pytest

# Each of these is only needed once a tool that uses it is called.
_DEFERRED = (
    "PIL",
    "bs4",
    "curl_cffi",
    "docx",
    "extruct",
    "greenlet",
    "httpx",
    "lxml",
    "markdownify",
    "openpyxl",
    "playwright",
    "pypdf",
    "rapidfuzz",
    "selectolax",
    "trafilatura",
    "search_mcp.agent",
    "search_mcp.aggregator",
    "search_mcp.documents",
    "search_mcp.engines",
    "search_mcp.fetcher",
    "search_mcp.structured",
)


def _modules_loaded_by(statement: str) -> set[str]:
    code = f"import json, sys\n{statement}\nprint(json.dumps(sorted(sys.modules)))"
    env = {**os.environ, "SEARCH_MCP_AGENT_BACKEND": "off"}
    out = subprocess.run(
        [sys.executable, "-c", code], capture_output=True, text=True, env=env, check=True
    )
    return set(json.loads(out.stdout.splitlines()[-1]))


def test_importing_the_server_loads_no_tool_dependency():
    loaded = _modules_loaded_by("import search_mcp.server")
    assert not {m for m in _DEFERRED if m in loaded}


def test_the_entry_point_loads_no_tool_dependency():
    loaded = _modules_loaded_by("import search_mcp.__main__")
    assert not {m for m in _DEFERRED if m in loaded}


async def test_a_lazy_tool_imports_its_implementation_on_call(monkeypatch):
    from search_mcp import aggregator, server

    async def fake(query, **kwargs):
        return {"query": query, **kwargs}

    monkeypatch.setattr(aggregator, "aggregate_search", fake)
    assert server.aggregate_search.__name__ == "aggregate_search"
    assert await server.aggregate_search("q", max_results=3) == {"query": "q", "max_results": 3}


def test_errors_of_loaded_modules_reach_the_model():
    import httpx

    from search_mcp import agent, server

    anticipated = server._anticipated()
    assert httpx.HTTPError in anticipated
    assert agent.AgentError in anticipated
    assert server._translate("fetch", httpx.ConnectTimeout("timed out")).args == ("timed out",)


@pytest.mark.parametrize("missing", ["httpx", "search_mcp.agent"])
def test_a_module_not_loaded_is_not_consulted(monkeypatch, missing):
    from search_mcp import server

    monkeypatch.delitem(sys.modules, missing)
    names = {t.__name__ for t in server._anticipated()}
    assert "BrowserLaunchError" in names
    assert ("HTTPError" in names) == (missing != "httpx")
    assert ("AgentError" in names) == (missing != "search_mcp.agent")


async def test_prewarm_loads_the_search_path(monkeypatch):
    from search_mcp import prewarm as prewarm_mod
    from search_mcp.config import settings

    imported: list[str] = []
    monkeypatch.setattr(settings, "prewarm", True)
    monkeypatch.setattr(prewarm_mod.importlib, "import_module", imported.append)
    monkeypatch.setattr("search_mcp.aggregator.default_pool_now", lambda: [])
    await prewarm_mod.prewarm()
    assert imported == ["search_mcp.aggregator", "search_mcp.fetcher"]
