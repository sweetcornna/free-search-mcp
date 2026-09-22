#!/usr/bin/env python3
"""End-to-end smoke test: start the server over stdio and call every tool.

    uv run python scripts/smoke_mcp.py            # both protocol eras
    uv run python scripts/smoke_mcp.py --era legacy

This is the check the unit suite cannot be. It starts THIS checkout's server as
a subprocess, speaks real MCP to it over stdio, and hits the live web — so it
exercises the SDK that is actually installed, the argument validation a client
goes through, and the engines as they are today. It is how "every tool can be
called" gets verified before a release, and it is not run in CI: it needs the
network, and a search engine having a bad afternoon should not fail a build.

Exit status is non-zero if any check fails.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

from mcp import Client
from mcp.client.stdio import StdioServerParameters
from mcp.types import PromptReference

ROOT = Path(__file__).resolve().parents[1]
ERAS = ["2026-07-28", "legacy"]
PAGE = "https://example.com/"


def _text(result: Any) -> str:
    return "\n".join(b.text for b in result.content if getattr(b, "type", "") == "text")


def _calls(doc: Path) -> list[tuple[str, dict[str, Any], str]]:
    """(tool, arguments, a substring the markdown answer must contain)."""
    return [
        ("engines", {"group": "web"}, "duckduckgo"),
        ("search", {"query": "python asyncio taskgroup exception handling", "max_results": 5},
         "# Search:"),
        ("fetch", {"url": PAGE}, "Example Domain"),
        ("fetch_batch", {"urls": [PAGE, "https://www.iana.org/help/example-domains"]},
         "Example Domain"),
        ("read_doc", {"source": str(doc)}, "smoke test document"),
        ("research", {"question": "what is the model context protocol", "depth": 1},
         "# Research brief:"),
        ("paper_graph", {"paper": "10.48550/arXiv.1706.03762", "limit": 3}, "Attention"),
        ("cache_search", {"query": "example"}, "example"),
        ("compare", {"question": "what is this domain for?",
                     "urls": [PAGE, "https://www.iana.org/help/example-domains"]}, "example"),
        ("extract_structured", {"url": "https://www.python.org/"}, "# Structured data:"),
        ("download", {"url": "https://www.python.org/static/favicon.ico"}, "favicon"),
    ]  # fmt: skip


async def _run(era: str) -> list[tuple[str, bool, str]]:
    rows: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        rows.append((name, ok, detail))

    with tempfile.TemporaryDirectory() as tmp:
        doc = Path(tmp) / "note.txt"
        doc.write_text("This is the smoke test document.\nSecond line.\n", encoding="utf-8")
        server = StdioServerParameters(
            command=sys.executable,
            args=["-m", "search_mcp"],
            cwd=str(ROOT),
            env={
                "PYTHONPATH": str(ROOT / "src"),
                "SEARCH_MCP_DOCUMENT_ROOT": tmp,
                "SEARCH_MCP_DOWNLOAD_DIR": str(Path(tmp) / "downloads"),
                "SEARCH_MCP_CACHE_DIR": str(Path(tmp) / "cache"),
                "SEARCH_MCP_LOG_LEVEL": "WARNING",
            },
        )
        async with Client(server, mode=era, read_timeout_seconds=120) as client:
            listed = [t.name for t in (await client.list_tools()).tools]
            expected = [name for name, _, _ in _calls(doc)]
            check("tools/list", sorted(listed) == sorted(expected), f"{len(listed)} tools")

            for name, args, needle in _calls(doc):
                started = time.monotonic()
                try:
                    result = await client.call_tool(name, args)
                except Exception as exc:
                    check(name, False, f"{type(exc).__name__}: {exc}")
                    continue
                body = _text(result)
                ok = (
                    not result.is_error
                    and needle.lower() in body.lower()
                    and result.structured_content is None
                    and not body.lstrip().startswith("{")
                )
                detail = f"{time.monotonic() - started:4.1f}s"
                if not ok:
                    detail += "  " + " ".join(body.split())[:160]
                check(name, ok, detail)

            # The json contract, once: structured, unwrapped, mirrored as text.
            as_json = await client.call_tool("engines", {"group": "news", "format": "json"})
            structured = as_json.structured_content
            check(
                "format=json",
                isinstance(structured, dict)
                and "taxonomy" in structured
                and json.loads(_text(as_json)) == structured,
            )

            # An actionable error has to arrive with its message.
            empty = await client.call_tool("search", {"query": "   "})
            check(
                "error message reaches the client",
                empty.is_error and "query must not be empty" in _text(empty),
                " ".join(_text(empty).split())[:120],
            )

            completed = await client.complete(
                PromptReference(type="ref/prompt", name="news_brief"),
                {"name": "since", "value": "w"},
            )
            check("completion", completed.completion.values == ["week"])

            templates = await client.list_resource_templates()
            uris = sorted(t.uri_template for t in templates.resource_templates)
            check("resource templates", uris == ["cache://page/{url}", "cache://search/{query_hash}"])

            from urllib.parse import quote

            page = await client.read_resource(f"cache://page/{quote(PAGE, safe='')}")
            check("resources/read", "Example Domain" in page.contents[0].text)
    return rows


async def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--era", choices=ERAS, action="append", help="default: both")
    args = parser.parse_args()

    failed = 0
    for era in args.era or ERAS:
        print(f"\n== protocol era {era}")
        for name, ok, detail in await _run(era):
            failed += not ok
            print(f"  {'PASS' if ok else 'FAIL'}  {name:34} {detail}")
    print(f"\n{'all checks passed' if not failed else f'{failed} check(s) FAILED'}")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
