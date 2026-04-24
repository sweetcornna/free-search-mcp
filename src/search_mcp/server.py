"""MCP server entry point. Tool docstrings are written for an LLM to read:
each tool says when to use it, when NOT to use it, what it returns, and the
mistakes models commonly make when calling it."""
from __future__ import annotations

import logging
from typing import Any, Literal

from mcp.server.fastmcp import Context, FastMCP
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


def _max_age_to_seconds(max_age_hours: float | None) -> int | None:
    if max_age_hours is None:
        return None
    return int(max_age_hours * 3600)


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
    max_age_hours: float | None = None,
    format: Format = "markdown",
) -> str | dict[str, Any]:
    """Run a multi-engine web search and return a ranked, deduplicated link list.

    Best for:
    - Discovery queries ("what is X", "find me X", "who is X").
    - Getting a list of URLs you can hand to `fetch` / `fetch_batch` next.
    - Topics likely to be after your knowledge cutoff.

    Not recommended for:
    - You already know the URL -> use `fetch` instead.
    - You want both links AND their full text in one call -> use `research`.
    - You want to query pages already in the local cache -> use `cache_search`.
    - Reading PDFs/DOCX from a known URL -> use `read_doc`.

    Returns:
    - markdown (default): numbered list of `n. title`, `<url>`, snippet — ~40%
      fewer tokens than json.
    - json: dict with `results` (list of {title,url,snippet,engines,score}),
      `engines`, `cached`, optional `errors` map, optional `hint` string.

    Common mistakes:
    - Passing a URL as `query` — that's `fetch`'s job.
    - Cranking `max_results` to 50 hoping for better recall; engines cap around
      10-20 each, anything beyond is duplicate noise.
    - Adding `engines=["brave","bing","baidu"]` by default — those need
      captcha-friendly conditions; stick with defaults unless they returned 0.

    Args:
        query: Natural-language query (the same string a human would type).
        engines: Subset of `engines()`. None = duckduckgo+mojeek+startpage.
        max_results: Merged result count after dedup. 5-20 is the useful range.
        use_cache: Reuse the last result for this exact (query, engines,
            max_results) within the cache TTL. False forces a re-fetch.
        max_age_hours: Treat cached results older than this as a miss. Use
            0 to force-refresh while keeping cache writes; None = use server
            default TTL (7 days).
        format: "markdown" (default) or "json".
    """
    if not query.strip():
        raise ValueError("query must not be empty")

    max_age_seconds = _max_age_to_seconds(max_age_hours)
    effective_use_cache = use_cache
    cache_hit: list[dict[str, Any]] | None = None

    if use_cache and max_age_seconds is not None:
        # Pre-check cache with a tighter TTL ourselves; if it misses we tell
        # the aggregator not to use cache so it re-runs.
        from .aggregator import _key  # local import: aggregator owns the key shape
        engine_names = engines or settings.default_engines
        n = max_results or settings.max_results_per_engine
        key = _key(query, engine_names, n)
        if max_age_seconds == 0:
            cache_hit = None
        else:
            cache_hit = await cache.get_search(key, max_age_seconds=max_age_seconds)
        if cache_hit is None:
            effective_use_cache = False

    if cache_hit is not None:
        payload: dict[str, Any] = {
            "query": query,
            "engines": engines or settings.default_engines,
            "cached": True,
            "results": cache_hit,
        }
    else:
        payload = await aggregate_search(
            query,
            engines=engines,
            max_results=max_results,
            use_cache=effective_use_cache,
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
    max_age_hours: float | None = None,
    format: Format = "markdown",
) -> str | dict[str, Any]:
    """Fetch one URL and return reader-mode Markdown of the main content.

    Best for:
    - You already have a URL (from `search`, the user, or your own knowledge)
      and need the actual page text.
    - Verifying a single claim by reading the source.
    - Pages that need reader-mode cleanup (nav/footer/scripts stripped).

    Not recommended for:
    - Multiple URLs at once -> use `fetch_batch` (concurrent, one round-trip).
    - "Search then read top N" -> use `research` (one call, not two).
    - PDF/DOCX URLs -> use `read_doc` (proper binary parsing).
    - You don't have a URL yet -> use `search` first.

    Returns:
    - markdown (default): a small header (URL, render method, token count)
      plus the cleaned page body.
    - json: {url, title, content, method, truncated, tokens_estimated,
      author, published_date, sitename}.

    Common mistakes:
    - Passing a search query instead of a URL.
    - Using `render="http"` on a JS-only SPA — it returns near-empty content;
      use "auto" (default) or "browser".
    - Forgetting that results are cached 7 days — use `force_refresh=True`
      or `max_age_hours=0` for a fresh pull.

    Args:
        url: Absolute http(s) URL.
        render: "auto" (try HTTP, fall back to stealth Chromium), "http"
            (fast, fails on JS), "browser" (slow, robust).
        force_refresh: Bypass the page cache entirely.
        max_age_hours: Treat cached pages older than this as a miss. 0 = same
            as force_refresh. None = server default TTL (7 days).
        format: "markdown" or "json".
    """
    effective_force = force_refresh
    if max_age_hours is not None:
        if max_age_hours == 0:
            effective_force = True
        else:
            cached = await cache.get_page(
                url, max_age_seconds=_max_age_to_seconds(max_age_hours),
            )
            if cached is None:
                effective_force = True
    result = await fetch_page(url, render=render, force_refresh=effective_force)
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
    ctx: Context | None = None,
) -> str | list[dict[str, Any]]:
    """Fetch a list of URLs in parallel. Per-URL failures do not raise.

    Best for:
    - 2+ URLs you want to read in one round-trip.
    - Reading the top N results of a previous `search` call.

    Not recommended for:
    - A single URL -> `fetch` (no list-wrapping overhead).
    - "Search and then read" -> `research` collapses both into one tool call.
    - PDFs/DOCX -> `read_doc` per file.

    Returns:
    - markdown (default): each page rendered as a Markdown section, separated
      by horizontal rules; failed URLs become inline error notes.
    - json: list[dict], one entry per URL, with `error` set on failures.

    Common mistakes:
    - Passing a single URL inside a 1-element list — use `fetch` directly.
    - Assuming an exception means the whole batch failed; check each item's
      `error` field instead.

    Args:
        urls: List of absolute http(s) URLs.
        render: Same as `fetch`.
        format: "markdown" or "json".
    """
    if not urls:
        return "" if format == "markdown" else []
    if ctx is not None:
        await ctx.report_progress(0.0, float(len(urls)), "starting batch fetch")
    raw = await fetch_many(urls, render=render)
    items: list[dict[str, Any]] = []
    for idx, r in enumerate(raw, 1):
        items.append(r.to_dict() if hasattr(r, "to_dict") else r)
        if ctx is not None:
            await ctx.report_progress(float(idx), float(len(urls)), f"fetched {idx}/{len(urls)}")
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
    """Read a local file or http(s) document into Markdown.

    Best for:
    - Local or remote PDFs and DOCX (parsed locally, no remote API).
    - Local text/HTML/Markdown files the user pointed at.
    - Paginating through a long document via `start` / `length`.

    Not recommended for:
    - Arbitrary HTML web pages -> `fetch` does reader-mode cleanup that this
      tool does not.
    - Pages discovered through search -> `fetch` or `research`.

    Returns:
    - markdown (default): rendered document text with a small header.
    - json: {content, title, format, total_chars, start, returned_chars,
      truncated}. Use `total_chars` and `returned_chars` to drive pagination.

    Common mistakes:
    - Calling this on a normal article URL — you'll get raw HTML noise; use
      `fetch` instead.
    - Forgetting to advance `start` when paginating: next call should pass
      `start = previous_start + returned_chars`.

    Args:
        source: Local path (e.g. "~/papers/x.pdf") or http(s) URL.
        start: Character offset to begin reading from. Default 0.
        length: Max characters to return; None = read to end (still capped
            by per-call max content size).
        format: "markdown" or "json".
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
    max_age_hours: float | None = None,
    format: Format = "markdown",
    ctx: Context | None = None,
) -> str | dict[str, Any]:
    """One-shot research: search the web, fetch the top results, return both.

    Best for:
    - Open-ended questions that need finding sources AND reading them
      ("what's new with X", "summarize the controversy around Y").
    - Replacing a `search` + N x `fetch` chain with one call.
    - Producing a citable brief with [n]-style source references.

    Not recommended for:
    - You only need links -> `search` (cheaper, no fetching).
    - You only need to read one URL you already have -> `fetch`.
    - You want to query previously-fetched cached pages -> `cache_search`.

    Returns:
    - markdown (default): a "Research brief" with a Sources index then the
      full Markdown body of each fetched document, separated by horizontal
      rules; includes a token estimate.
    - json: {question, engines, sources:[{rank,title,url,snippet,...}],
      documents:[...], tokens_estimated, errors}.

    Common mistakes:
    - Using `depth=8` for a quick lookup — that's 8 page fetches; 2-3 is
      almost always enough.
    - Calling `research` for a known URL — that's `fetch` territory.
    - Forgetting that `fetch=False` returns sources only (much cheaper if
      the LLM only needs to pick which one to read).

    Args:
        question: What you want to know, in natural language.
        depth: How many top results to fetch (1-8). 3 is a good default.
        engines: Override the engine set (see `engines()` for names).
        fetch: If False, return source list without reading them.
        use_cache: Reuse cached search/page data within TTL.
        max_age_hours: Treat cached search results older than this as a miss
            (0 = force-refresh search; None = server default TTL).
        format: "markdown" or "json".
    """
    if ctx is not None:
        await ctx.report_progress(0.05, 1.0, "starting research")

    # Translate max_age_hours -> use_cache for the search portion.
    effective_use_cache = use_cache
    if max_age_hours is not None and max_age_hours == 0:
        effective_use_cache = False

    if ctx is not None:
        await ctx.report_progress(0.15, 1.0, "searching engines")

    payload = await run_research(
        question,
        depth=depth,
        engines=engines,
        fetch=fetch,
        use_cache=effective_use_cache,
    )

    if ctx is not None:
        # Coarse end-of-fetch milestones — research.py runs fetch_many internally
        # so we can't checkpoint per-URL without rewriting it.
        n_docs = max(1, len(payload.get("documents") or [1]))
        await ctx.report_progress(0.95, 1.0, f"fetched {n_docs} sources")
        await ctx.report_progress(1.0, 1.0, "done")

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
    """Full-text search over pages already fetched into the local SQLite FTS5 index.

    Best for:
    - Recalling something the user/agent fetched earlier in the conversation
      ("what did that Wikipedia page say about X").
    - Avoiding re-fetching content already in the local cache.
    - Quick keyword grep across the corpus you've built up.

    Not recommended for:
    - Discovering new pages on the open web -> use `search` or `research`.
    - When the cache is empty (fresh install) -> `search`/`research` first to
      populate it.

    Returns:
    - markdown (default): a per-hit list of title, URL, and a `[bracket]`-
      highlighted snippet around the matched terms.
    - json: list of {url, title, snippet}.

    Common mistakes:
    - Treating this like web search — it ONLY hits pages already in the local
      cache. If the user hasn't fetched anything, you'll get zero hits.
    - Using natural-language phrases without quoting them; FTS5 splits on
      whitespace as AND. For an exact phrase use `"like this"`.

    Args:
        query: FTS5 query. Bare terms = AND. Supports OR / NOT, prefix
            (`term*`), and phrase (`"exact phrase"`).
        limit: Max hits to return.
        format: "markdown" or "json".
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
    """List engine names accepted by the `engines=` parameter of `search` / `research`.

    Best for:
    - Discovering what's installable before passing a non-default engine.
    - Building user-facing UIs that let humans pick engines.

    Not recommended for:
    - Calling on every search — the list is static; cache it.

    Returns:
    - A list of engine name strings (e.g. ["duckduckgo", "mojeek",
      "startpage", "brave", "bing", "baidu"]).

    Common mistakes:
    - Passing one of these names as a query to `search` — they go in the
      `engines=` argument, not `query`.

    Defaults: duckduckgo + mojeek + startpage (all reliable, no captchas).
    Opt-in:   brave (PoW captcha after a few calls), bing (UA-gated),
              baidu (results wrapped in baidu.com/link redirects).
    """
    return list_engines()


# ---------------------------------------------------------------------------
# Prompts (slash-commands in MCP clients)
# ---------------------------------------------------------------------------


@mcp.prompt(title="Research thoroughly")
def research_prompt(question: str, depth: int = 3) -> str:
    """Instruct the model to do a thorough, cited research pass on a question."""
    return (
        f"You have access to the search-mcp tools. Research the following "
        f"question thoroughly and produce a well-cited answer.\n\n"
        f"QUESTION: {question}\n\n"
        f"PROCEDURE:\n"
        f"1. Call the `research` tool with question={question!r} and depth={depth}.\n"
        f"2. Read each fetched source. If a source seems unreliable, call "
        f"`search` for a corroborating source.\n"
        f"3. If any document was truncated, call `fetch` again with that URL "
        f"or use `read_doc` for paginating PDFs.\n"
        f"4. Write a synthesis (3-8 paragraphs) that:\n"
        f"   - Answers the question directly in the first sentence.\n"
        f"   - Cites sources inline using [1], [2], ... markers that match the\n"
        f"     order returned by `research`.\n"
        f"   - Notes any disagreement between sources.\n"
        f"   - Lists the full source URLs at the end under a 'Sources' header.\n"
        f"5. If you could not find a confident answer, say so explicitly and\n"
        f"   show what was checked."
    )


@mcp.prompt(title="Fact-check claim")
def factcheck_prompt(claim: str) -> str:
    """Instruct the model to fact-check a specific claim with citations."""
    return (
        f"Fact-check the following claim using the search-mcp tools.\n\n"
        f"CLAIM: {claim}\n\n"
        f"PROCEDURE:\n"
        f"1. Call `search` with a focused query (key entities + date if any).\n"
        f"2. Call `fetch_batch` on the 3-5 most authoritative-looking URLs\n"
        f"   (prefer primary sources, official sites, established outlets).\n"
        f"3. For each source, quote the supporting or contradicting passage.\n"
        f"4. Output a verdict on a 5-point scale: TRUE / MOSTLY TRUE / MIXED /\n"
        f"   MOSTLY FALSE / FALSE, followed by a one-paragraph justification\n"
        f"   with [n]-style citations matching the source order.\n"
        f"5. End with a 'Sources' list of URLs.\n"
        f"6. If sources disagree, surface that explicitly rather than picking\n"
        f"   one side silently."
    )


# ---------------------------------------------------------------------------
# Resource template — expose cached pages as readable resources
# ---------------------------------------------------------------------------


@mcp.resource("cache://page/{url}", title="Cached page")
async def cached_page(url: str) -> str:
    """Return the cached Markdown body for a previously-fetched URL.

    The URL must be percent-encoded when embedded in the resource URI
    (RFC 6570 templates do not allow `:` or `/` inside variable expansions).
    """
    from urllib.parse import unquote
    decoded = unquote(url)
    page = await cache.get_page(decoded)
    if not page:
        raise ValueError(f"Not in cache: {decoded}")
    return page.get("content") or ""


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
