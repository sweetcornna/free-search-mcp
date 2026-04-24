"""MCP server entry point. Tool docstrings are written for an LLM to read:
each tool says when to use it, when NOT to use it, what to do when it fails,
and how it composes with the others."""
from __future__ import annotations

import logging
from typing import Any, Literal

from mcp.server.fastmcp import FastMCP
from mcp.types import ToolAnnotations

from .aggregator import aggregate_search, list_engines
from .browser import pool
from .cache import cache
from .config import settings
from .documents import read_document
from .fetcher import fetch_many, fetch_page
from .formatting import (
    estimate_tokens,
    errors_to_hint,
    render_doc,
    render_fetch,
    render_research,
    render_search,
)
from .research import research as run_research

logging.basicConfig(
    level=getattr(logging, settings.log_level.upper(), logging.INFO),
    format="%(asctime)s %(levelname)s %(name)s %(message)s",
)

mcp = FastMCP("search-mcp")

Format = Literal["markdown", "json"]

# ToolAnnotations meaning recap (for the maintainers, not for the LLM):
#   readOnlyHint  - the call does not change server state visible to others
#   idempotentHint - same args within a session yield the same result
#   openWorldHint  - the tool reaches outside the server (network, real world)


def _maybe_render(payload: dict[str, Any], fmt: Format, renderer) -> str | dict[str, Any]:
    if fmt == "json":
        return payload
    return renderer(payload)


@mcp.tool(
    annotations=ToolAnnotations(
        title="Web search (multi-engine, no API key)",
        readOnlyHint=True,
        idempotentHint=False,
        openWorldHint=True,
    ),
)
async def search(
    query: str,
    engines: list[str] | None = None,
    max_results: int = 10,
    use_cache: bool = True,
    format: Format = "markdown",
) -> str | dict[str, Any]:
    """Run a multi-engine web search and return a ranked, deduplicated list.

    USE WHEN:
    - The user asks "what is X / who is X / search for X / find me X".
    - You need fresh information that may be after your knowledge cutoff.
    - You want links you can hand to `fetch` next.

    DO NOT USE WHEN:
    - You already have the URL — call `fetch` directly.
    - The user wants to combine search + read in one shot — call `research`.
    - You want to query *previously fetched* pages — call `cache_search`.

    OUTPUT:
    - format="markdown" (default): numbered list of titles, URLs, snippets.
      Compact, readable, ~40% fewer tokens than JSON.
    - format="json": structured dict with `results`, `engines`, `errors`.

    ARGS:
        query: Natural-language query. Engines accept the same string the user
            would type into a search box.
        engines: Subset of `engines()` to query in parallel. None = defaults
            (duckduckgo, mojeek, startpage). Pass `["brave"]`, `["bing"]`, or
            `["baidu"]` only if you need results none of the defaults could
            find — those engines challenge headless clients intermittently.
        max_results: Merged result count after dedup. 5-20 is the useful range.
        use_cache: Reuse the last result for this exact (query, engines,
            max_results) within the cache TTL. Pass False to force re-fetch.

    ON FAILURE:
    - Empty results -> rephrase, broaden, or add `engines=` with one not in the
      defaults. The response includes an `errors` map if individual engines
      blew up; the others still ran.
    """
    if not query.strip():
        raise ValueError("query must not be empty")
    payload = await aggregate_search(
        query, engines=engines, max_results=max_results, use_cache=use_cache,
    )
    hint = errors_to_hint(payload.get("errors"))
    if hint:
        payload["hint"] = hint
    return _maybe_render(payload, format, render_search)


@mcp.tool(
    annotations=ToolAnnotations(
        title="Fetch URL as Markdown",
        readOnlyHint=True,
        idempotentHint=True,
        openWorldHint=True,
    ),
)
async def fetch(
    url: str,
    render: Literal["auto", "http", "browser"] = "auto",
    force_refresh: bool = False,
    format: Format = "markdown",
) -> str | dict[str, Any]:
    """Fetch a single URL and return reader-mode Markdown of the main content.

    USE WHEN:
    - You have a URL (from `search`, the user, or your own knowledge) and need
      the actual page text.
    - You want to verify a claim by reading the source.

    DO NOT USE WHEN:
    - You have many URLs at once -> `fetch_batch` is concurrent.
    - You haven't searched yet and don't know URLs -> `search` or `research`.
    - The URL points to a PDF or DOCX -> `read_doc` parses those properly.

    OUTPUT:
    - format="markdown" (default): a small header (URL, render method, token
      count) plus the cleaned content.
    - format="json": dict with `content`, `title`, `method`, `truncated`,
      `tokens_estimated`.

    Boilerplate (nav/footer/scripts) is stripped. Result is cached for 7 days
    by URL — pass `force_refresh=True` to bypass.

    ARGS:
        url: Absolute http(s) URL.
        render: "auto" (default) tries plain HTTP first, falls back to a
            stealth headless Chromium if the page is JS-only or hostile.
            "http" forces no-browser (fast, fails on JS sites). "browser"
            forces Playwright (slow, works on most sites).
        force_refresh: Bypass the page cache.
    """
    result = await fetch_page(url, render=render, force_refresh=force_refresh)
    payload = result.to_dict()
    return _maybe_render(payload, format, render_fetch)


@mcp.tool(
    annotations=ToolAnnotations(
        title="Fetch many URLs concurrently",
        readOnlyHint=True,
        idempotentHint=True,
        openWorldHint=True,
    ),
)
async def fetch_batch(
    urls: list[str],
    render: Literal["auto", "http", "browser"] = "auto",
    format: Format = "markdown",
) -> str | list[dict[str, Any]]:
    """Fetch a list of URLs in parallel. Failures are reported per-URL, not raised.

    USE WHEN:
    - You have 2+ URLs you want to read.
    - You want one round-trip instead of N.

    For 1 URL use `fetch`. For "search and then read top N", `research` is one
    call instead of two.

    OUTPUT:
    - format="markdown" (default): each page rendered as a Markdown section,
      separated by horizontal rules. Failed URLs become inline error notes.
    - format="json": list[dict], one entry per URL, with `error` set on
      failures.
    """
    if not urls:
        return "" if format == "markdown" else []
    raw = await fetch_many(urls, render=render)
    items: list[dict[str, Any]] = []
    for r in raw:
        items.append(r.to_dict() if hasattr(r, "to_dict") else r)
    if format == "json":
        return items
    sections = []
    for it in items:
        if "error" in it:
            sections.append(f"### ⚠ {it.get('url', '')}\n_failed: {it['error']}_\n")
        else:
            sections.append(render_fetch(it))
    return "\n---\n\n".join(sections)


@mcp.tool(
    annotations=ToolAnnotations(
        title="Read a local or remote document",
        readOnlyHint=True,
        idempotentHint=True,
        openWorldHint=False,
    ),
)
async def read_doc(
    source: str,
    start: int = 0,
    length: int | None = None,
    format: Format = "markdown",
) -> str | dict[str, Any]:
    """Read a local file path or http(s) URL into Markdown.

    Supports PDF, DOCX, HTML, plain text, and Markdown — all parsed locally
    (no remote API). For arbitrary web pages use `fetch` instead; this tool
    is the right one for binary documents like PDFs.

    USE WHEN:
    - The source is a PDF/DOCX file (local OR a URL ending in .pdf/.docx).
    - The source is a local text/HTML file the user pointed at.

    PAGINATION:
    - Large documents are sliced. The response includes `total_chars`, `start`,
      `returned_chars`, and `truncated`. To read the next chunk, call again
      with `start=<previous start + returned_chars>`.

    ARGS:
        source: Local path (e.g. "~/papers/x.pdf") or http(s) URL.
        start: Character offset to start reading from. Default 0.
        length: Max characters to read. Default None (= read to end of doc,
            still subject to the per-call max content cap).
    """
    result = await read_document(source, start=start, length=length)
    payload = result.to_dict()
    return _maybe_render(payload, format, render_doc)


@mcp.tool(
    annotations=ToolAnnotations(
        title="Search and read in one call",
        readOnlyHint=True,
        idempotentHint=False,
        openWorldHint=True,
    ),
)
async def research(
    question: str,
    depth: int = 3,
    engines: list[str] | None = None,
    fetch: bool = True,
    use_cache: bool = True,
    format: Format = "markdown",
) -> str | dict[str, Any]:
    """One-shot research: search the web, fetch the top results, return both.

    USE WHEN:
    - The user asks a question that needs both finding sources AND reading them
      ("what's new with X", "summarize the controversy around Y", "find me a
      tutorial for Z").
    - You'd otherwise call `search` + `fetch` + `fetch` + `fetch`. This is
      one round-trip instead of four.

    DO NOT USE WHEN:
    - You only need a list of links -> `search` is cheaper.
    - You only need to read one URL you already have -> `fetch`.

    OUTPUT (format="markdown"):
    - A "Research brief" with a `Sources` index and full Markdown bodies of
      each fetched document, separated by horizontal rules. Includes a token
      estimate so you can decide whether to summarize.

    ARGS:
        question: What you want to know, in natural language.
        depth: How many top results to fetch (1-8). 3 is a good default.
        engines: Override the engine set (see `engines()` for names).
        fetch: If False, return source list without reading them (cheap).
        use_cache: Reuse cached search/page data within TTL.
    """
    payload = await run_research(
        question, depth=depth, engines=engines, fetch=fetch, use_cache=use_cache,
    )
    return _maybe_render(payload, format, render_research)


@mcp.tool(
    annotations=ToolAnnotations(
        title="Search local cache (FTS5)",
        readOnlyHint=True,
        idempotentHint=True,
        openWorldHint=False,
    ),
)
async def cache_search(
    query: str,
    limit: int = 10,
    format: Format = "markdown",
) -> str | list[dict[str, Any]]:
    """Full-text search over pages already fetched into the local SQLite FTS5
    index.

    USE WHEN:
    - The user asks about something you've previously fetched ("what did that
      Wikipedia page say about X").
    - You want to avoid re-fetching the same content.

    Query syntax is FTS5: bare terms AND-by-default, supports OR/NOT, prefix
    `term*`, and phrase `"exact phrase"`. Results include a snippet with the
    matched terms wrapped in [brackets].
    """
    rows = await cache.search_pages(query, limit=limit)
    if format == "json":
        return rows
    if not rows:
        return f"_No cached pages match `{query}`. Use `fetch` or `research` to populate the cache._\n"
    lines = [f"# Cache hits for `{query}`", ""]
    for r in rows:
        lines.append(f"## {r.get('title') or '(untitled)'}")
        lines.append(f"<{r.get('url')}>")
        sn = r.get("snippet")
        if sn:
            lines.append("")
            lines.append(f"> {sn}")
        lines.append("")
    return "\n".join(lines)


@mcp.tool(
    annotations=ToolAnnotations(
        title="List available search engines",
        readOnlyHint=True,
        idempotentHint=True,
        openWorldHint=False,
    ),
)
def engines() -> list[str]:
    """List engines you can pass to `search` / `research` via `engines=`.

    Defaults: duckduckgo + mojeek + startpage (all reliable, no captchas).
    Opt-in:   brave (PoW captcha after a few calls), bing (UA-gated),
              baidu (results wrapped in baidu.com/link redirects).
    """
    return list_engines()


def run() -> None:
    try:
        mcp.run()
    finally:
        try:
            import anyio
            anyio.run(pool.shutdown)
        except Exception:
            pass


__all__ = ["mcp", "run"]
