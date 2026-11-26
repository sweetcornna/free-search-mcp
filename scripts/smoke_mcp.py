#!/usr/bin/env python3
"""End-to-end stdio MCP smoke test for free-search-mcp 1.0 (Rust binary).

    uv run python scripts/smoke_mcp.py
    uv run python scripts/smoke_mcp.py --bin ./target/release/free-search-mcp --era legacy

Starts the compiled `free-search-mcp` binary over stdio, verifies the exact
3-tool surface (`ask`, `exec`, `search`), 0 prompts, 0 resource templates, and
runs live checks across all three tools. Not run in CI (hits the live network).

Exit status is non-zero if any check fails.
"""

from __future__ import annotations

import argparse
import asyncio
import shutil
import tempfile
import time
from pathlib import Path
from typing import Any

from mcp import Client
from mcp.client.stdio import StdioServerParameters

ROOT = Path(__file__).resolve().parents[1]
ERAS = ["2026-07-28", "legacy"]
PAGE = "https://example.com/"


def _text(result: Any) -> str:
    return "\n".join(b.text for b in result.content if getattr(b, "type", "") == "text")


def _resolve_binary(explicit: str | None) -> str:
    if explicit:
        return explicit
    candidates = [
        ROOT / "target" / "release" / "free-search-mcp",
        ROOT / "target" / "debug" / "free-search-mcp",
    ]
    for path in candidates:
        if path.is_file():
            return str(path)
    found = shutil.which("free-search-mcp")
    if found:
        return found
    raise SystemExit(
        "free-search-mcp binary not found; pass --bin /path/to/free-search-mcp "
        "or run `cargo build --release --locked -p fsm-server`."
    )


def _calls(doc: Path) -> list[tuple[str, str, dict[str, Any], str]]:
    """(label, tool, arguments, substring expected in text output)."""
    return [
        (
            "search:catalog",
            "search",
            {"query": "arxiv", "scope": "catalog", "limit": 5},
            "arxiv",
        ),
        (
            "search:web",
            "search",
            {"query": "rust tokio async runtime", "scope": "web", "limit": 5},
            "tokio",
        ),
        (
            "ask:fast",
            "ask",
            {"question": "tokio crate latest stable version", "mode": "fast"},
            "tokio",
        ),
        (
            "exec:web.fetch",
            "exec",
            {"code": f'web.fetch("{PAGE}")'},
            "Example Domain",
        ),
        (
            "search:cache",
            "search",
            {"query": "example", "scope": "cache", "limit": 5},
            "example",
        ),
        (
            "exec:doc.read",
            "exec",
            {"code": f"doc.read({str(doc)!r})"},
            "smoke test document",
        ),
        (
            "exec:scholar.work",
            "exec",
            {"code": 'scholar.work("arXiv:1706.03762")'},
            "Attention",
        ),
        (
            "exec:web.request.body",
            "exec",
            {
                "code": (
                    f'async () => {{ const r = await web.request({{ url: "{PAGE}" }}); '
                    'return { status: r.status, hasBody: r.body.includes("Example Domain"), '
                    'hasTextProp: "text" in r }; }'
                )
            },
            '"hasBody":true',
        ),
    ]


async def _run(binary: str, era: str) -> list[tuple[str, bool, str]]:
    rows: list[tuple[str, bool, str]] = []

    def check(name: str, ok: bool, detail: str = "") -> None:
        rows.append((name, ok, detail))

    with tempfile.TemporaryDirectory() as tmp:
        doc = Path(tmp) / "note.txt"
        doc.write_text("This is the smoke test document.\nSecond line.\n", encoding="utf-8")
        server = StdioServerParameters(
            command=binary,
            args=[],
            cwd=str(ROOT),
            env={
                "SEARCH_MCP_DOCUMENT_ROOT": tmp,
                "SEARCH_MCP_DOWNLOAD_DIR": str(Path(tmp) / "downloads"),
                "SEARCH_MCP_CACHE_DIR": str(Path(tmp) / "cache"),
                "SEARCH_MCP_OFFLINE_SYNC": "false",
                "SEARCH_MCP_LOG_LEVEL": "WARN",
            },
        )
        async with Client(server, mode=era, read_timeout_seconds=120) as client:
            listed = sorted(t.name for t in (await client.list_tools()).tools)
            check(
                "tools/list == [ask, exec, search]",
                listed == ["ask", "exec", "search"],
                str(listed),
            )

            for label, tool, args, needle in _calls(doc):
                started = time.monotonic()
                try:
                    result = await client.call_tool(tool, args)
                except Exception as exc:
                    check(label, False, f"{type(exc).__name__}: {exc}")
                    continue
                body = _text(result)
                ok = (
                    not result.is_error
                    and needle.lower() in body.lower()
                    and result.structured_content is None
                )
                detail = f"{time.monotonic() - started:4.1f}s"
                if not ok:
                    detail += "  " + " ".join(body.split())[:160]
                check(label, ok, detail)

            # Actionable error on unknown scope.
            bad = await client.call_tool("search", {"query": "test", "scope": "nonexistent_scope"})
            check(
                "unknown scope returns isError",
                bad.is_error and "scope" in _text(bad).lower(),
                " ".join(_text(bad).split())[:120],
            )
    return rows


async def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--bin", dest="binary", help="Path to free-search-mcp binary")
    parser.add_argument("--era", choices=ERAS, action="append", help="default: both")
    args = parser.parse_args()

    binary = _resolve_binary(args.binary)
    failed = 0
    for era in args.era or ERAS:
        print(f"\n== protocol era {era} ({binary})")
        for name, ok, detail in await _run(binary, era):
            failed += not ok
            print(f"  {'PASS' if ok else 'FAIL'}  {name:34} {detail}")
    print(f"\n{'all checks passed' if not failed else f'{failed} check(s) FAILED'}")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(asyncio.run(main()))
