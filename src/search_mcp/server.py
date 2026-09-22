"""MCP server entry point. Tool docstrings are written for an LLM to read:
each tool says when to use it, when NOT to use it, what it returns, and the
mistakes models commonly make when calling it."""
from __future__ import annotations

import functools
import inspect
import json
import logging
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any, Literal, get_args
from urllib.parse import quote, unquote

import httpx
import pydantic_core
from mcp.server.caching import CacheHint
from mcp.server.mcpserver import Context, Image, MCPServer
from mcp.server.mcpserver.exceptions import ResourceNotFoundError, ToolError
from mcp.server.transport_security import TransportSecuritySettings
from mcp.types import (
    CallToolResult,
    Completion,
    Icon,
    PromptReference,
    ResourceLink,
    ResourceTemplateReference,
    TextContent,
    ToolAnnotations,
)

from . import __version__, downloads
from .agent import HOST_AGENT_PROMPT, AgentError, config_problem
from .agent import ask as run_ask
from .aggregator import aggregate_search, list_engines
from .browser import BrowserUnavailableError, pool
from .cache import cache
from .compare import compare_urls
from .config import settings
from .documents import read_document
from .engines import ENGINES, Category, source_taxonomy
from .engines.base import Freshness
from .fetcher import decode_cached_title, fetch_bytes, fetch_many, fetch_page
from .formatting import (
    errors_to_hint,
    render_ask,
    render_compare,
    render_doc,
    render_engines,
    render_fetch,
    render_paper_graph,
    render_research,
    render_search,
    render_structured,
)
from .httpfetch import FetchError, MaxBytesExceededError
from .keystore import opt_in_engines
from .paper_graph import paper_graph as run_paper_graph
from .research import research as run_research
from .structured import extract_structured as _extract_structured

logging.basicConfig(
    level=getattr(logging, settings.log_level.upper(), logging.INFO),
    format="%(asctime)s %(levelname)s %(name)s %(message)s",
)

# Protocol-level logging (`notifications/message`) is deprecated as of the
# 2026-07-28 revision; stderr via the stdlib is the recommended replacement,
# which is what basicConfig above already gives us on stdio.
log = logging.getLogger(__name__)

# Every list here is fixed for the life of the process — tools, prompts and
# resource templates are all declared at import time and nothing mutates them.
# Saying so lets clients cache and stop re-listing. `resources/read` is the
# exception: it serves this machine's page cache, so it is per-user and short.
_STATIC_LIST = CacheHint(ttl_ms=3_600_000, scope="public")
_CACHE_HINTS = {
    # Identity and capabilities: as static as the lists, and asked for first.
    "server/discover": _STATIC_LIST,
    "tools/list": _STATIC_LIST,
    "prompts/list": _STATIC_LIST,
    "resources/list": _STATIC_LIST,
    "resources/templates/list": _STATIC_LIST,
    "resources/read": CacheHint(ttl_ms=60_000, scope="private"),
}

# Keyword args throughout: MCPServer's positional order is
# (name, title, description, instructions, ...), so a positional `instructions`
# would silently land in the `title` slot.
mcp = MCPServer(
    "search-mcp",
    title="Free Search",
    description=(
        "Keyless multi-engine web search, page fetching and document reading. "
        "Runs locally; no account, no API key."
    ),
    # A URL, never a data: URI — in the 2026-07-28 era serverInfo rides along in
    # the `_meta` of every result, so an inlined PNG would be paid for per call.
    icons=[
        Icon(
            src="https://raw.githubusercontent.com/sweetcornna/free-search-mcp/main/assets/icon.png",
            mime_type="image/png",
            sizes=["128x128"],
        )
    ],
    # Sent to every client on connect, so it is kept to what changes behaviour.
    # Plugin users also get the `verified-research` skill; everyone else has
    # only this, which is why the two rules that matter most are here: read
    # details from the page, and check how old the output says it is.
    instructions=(
        "Keyless multi-engine web search, page fetching and document reading. No API "
        "key is needed, so never ask the user for one. `search` finds URLs. Its snippets "
        "are summaries and may be out of date, so read dates, amounts, rules and other "
        "details from the page itself: `fetch` / `fetch_batch` for known URLs, `research` "
        "for search plus reading in one call, `read_doc` for paginated PDF/DOCX, and "
        "`extract_structured` for dates and prices as fields. Results say when they were "
        "retrieved, how old a cached copy is, and which results are undated. Check that "
        "before calling anything current, and pull again with `force_refresh=True` or "
        "`max_age_hours=0` when it matters. Every tool takes format='markdown' (default, "
        "compact) or format='json' (structured)."
    ),
    website_url="https://github.com/sweetcornna/free-search-mcp",
    version=__version__,
    cache_hints=_CACHE_HINTS,
)

Format = Literal["markdown", "json"]
# The top level of `source_taxonomy()`. A Literal, so the input schema carries an
# enum and a typo is a validation error that lists the choices — it used to be a
# silently empty tree. MCP has no completion for TOOL arguments (completion refs
# are prompts and resource templates only); an enum is the equivalent.
# tests/test_tool_schemas.py asserts this stays equal to the taxonomy's keys.
EngineGroup = Literal[
    "web", "news", "paper", "github", "forum", "image", "dataset", "finance",
    "software", "security", "reference", "weather", "docs", "gov", "stats", "calendar",
]

# ToolAnnotations meaning recap (for the maintainers, not for the LLM):
#   read_only_hint   - the call does not change server state visible to others
#   idempotent_hint  - same args yield the same result
#   open_world_hint  - the tool reaches outside the server (network, real world)


def _maybe_render(payload: dict[str, Any], fmt: Format, renderer) -> str | dict[str, Any]:
    if fmt == "json":
        return payload
    return renderer(payload)


def _max_age_to_seconds(max_age_hours: float | None) -> int | None:
    if max_age_hours is None:
        return None
    return int(max_age_hours * 3600)


# FTS5 boolean keywords are only valid as INFIX operators between two terms.
_FTS_OPERATORS = {"AND", "OR", "NOT", "NEAR"}


def _invalid_fts_hint(query: str) -> str | None:
    """Return a sanitized hint if `query` is malformed FTS5 syntax, else None.

    The cache layer already swallows the SQLite OperationalError and returns []
    (so we never leak raw SQL), but an empty result then looks identical to a
    legitimate "no pages matched". This heuristic detects the common syntax
    mistakes an LLM makes so the tool can explain *why* it got nothing, without
    re-running SQL or echoing SQLite's error text.
    """
    q = query.strip()
    if not q:
        return None  # empty query is "no input", not "bad syntax"
    # Unbalanced double quotes -> unterminated phrase.
    if q.count('"') % 2 == 1:
        return (
            "Your query has an unterminated quote. FTS5 phrases need matching "
            'double quotes, e.g. `"exact phrase"`.'
        )
    # Unbalanced parentheses.
    if q.count("(") != q.count(")"):
        return (
            "Your query has unbalanced parentheses. Group sub-expressions like "
            "`(a OR b) c`."
        )
    tokens = q.split()
    upper = [t.upper() for t in tokens]
    # A boolean operator may not lead or trail the expression.
    if upper[0] in _FTS_OPERATORS or upper[-1] in _FTS_OPERATORS:
        return (
            "Your query starts or ends with a boolean operator (AND/OR/NOT/NEAR). "
            "These join two terms, e.g. `cats AND dogs`, not `cats AND`."
        )
    # Two boolean operators in a row (e.g. `a AND OR b`).
    for prev, cur in zip(upper, upper[1:], strict=False):
        if prev in _FTS_OPERATORS and cur in _FTS_OPERATORS:
            return (
                "Your query has two boolean operators in a row. Put a term "
                "between them, e.g. `a AND b OR c`."
            )
    return None


async def _safe_progress(
    ctx: Context | None, current: float, total: float, message: str,
) -> None:
    """report_progress() raises when called from non-MCP contexts (unit tests,
    ad-hoc scripts, or clients that didn't pass a progressToken). Swallow that
    so progress is a nice-to-have, not a crash trigger.

    The catch is deliberately broad: the exception type is SDK plumbing that
    has already changed once across protocol revisions, and no failure to emit
    a progress ping is worth losing an otherwise-complete search result over.
    """
    if ctx is None:
        return
    try:
        await ctx.report_progress(current, total, message)
    except Exception:
        log.debug("progress notification dropped", exc_info=True)


# --- the tool boundary -----------------------------------------------------
#
# Since MCP SDK 2.1 a tool's exception reaches the client with its message only
# when it is a `ToolError`. Anything else is treated as a crash: the traceback
# goes to the server log and the model is told "Error executing tool <name>" —
# nothing about WHAT was wrong. That is the right default for a genuine bug and
# the wrong one for everything this server raises on purpose: "query must not
# be empty", "compare expects 2-5 URLs", "Refusing to connect to blocked
# address", "downloads are disabled (SEARCH_MCP_DOWNLOAD_ENABLED=false)". Those
# messages exist to let the model correct its own call, and 0.11.0 shipped them
# into a void — CI ran SDK 2.0 (where every message was forwarded) while `uvx`
# resolved 2.2 for every real install.
#
# The translation lives HERE, at registration, rather than at each raise site:
#   * compare.py / documents.py / downloads.py / url_safety.py stay free of any
#     MCP import and keep raising the builtin types their own tests assert on
#     (`pytest.raises(ValueError)`, `pytest.raises(PermissionError)`);
#   * the module-level tool functions stay raw too (`_tool` returns the original
#     function), so a direct Python caller gets native exceptions and native
#     return values, exactly as before;
#   * one place decides what the model may be told.
#
# The list is explicit on purpose. Allow-listing `Exception` would forward the
# text of real bugs — a KeyError's key, an AttributeError naming an internal —
# which is both useless to the model and the leak the SDK change set out to
# stop. `RuntimeError` is deliberately NOT here for the same reason; the fetch
# path's intentional failures carry the narrower `FetchError` instead.
_ANTICIPATED: tuple[type[BaseException], ...] = (
    # Argument checks here and in compare/documents/downloads. UnsafeURLError
    # (SSRF refusals) and EngineKeyError subclass it.
    ValueError,
    # PermissionError (sandbox / feature disabled), FileNotFoundError, and the
    # ConnectionError / TimeoutError family.
    OSError,
    FetchError,
    MaxBytesExceededError,
    BrowserUnavailableError,
    # read_doc, download and extract_structured fetch over httpx.
    httpx.HTTPError,
    httpx.InvalidURL,
    # `ask` with a backend that cannot run as configured. The message names the
    # setting to change.
    AgentError,
)


def _message(exc: BaseException) -> str:
    # Some network errors stringify to "" (httpx.ConnectTimeout does); a bare
    # "Error executing tool fetch: " would be the old problem all over again.
    text = str(exc).strip()
    return text or type(exc).__name__


def _translate(name: str, exc: Exception) -> ToolError:
    if isinstance(exc, ToolError):
        return exc
    if isinstance(exc, _ANTICIPATED):
        return ToolError(_message(exc))
    # A bug. Log everything, tell the model only what helps it decide what to
    # do next: this is not about its arguments, so retrying unchanged is wasted.
    log.exception("tool %s crashed", name)
    return ToolError(
        f"internal error ({type(exc).__name__}) - a server bug, not a problem "
        "with your arguments; retrying the same call will not help. Details "
        "are in the server log."
    )


@dataclass
class _Linked:
    """A tool result plus resource links to attach to it.

    Only ever returned when the tool was given a request `Context`, i.e. when it
    is running behind the boundary below, which unpacks it. A direct Python
    caller passes no context and gets the plain value, as before.
    """

    body: Any
    links: list[ResourceLink] = field(default_factory=list)


# `resource_link` content blocks arrived in protocol revision 2025-06-18. The
# SDK does not downgrade content for older clients, so an unknown block type
# would reach them as-is; revision strings are ISO dates and compare as such.
_LINKS_SINCE = "2025-06-18"


def _supports_links(ctx: Context | None) -> bool:
    if ctx is None:
        return False
    try:
        version = ctx.protocol_version
    except Exception:
        return False
    return isinstance(version, str) and version >= _LINKS_SINCE


def _to_result(value: Any) -> Any:
    """Shape a tool's return value into what actually goes on the wire.

    Every tool here is dual-format: `format="markdown"` returns a `str`,
    `format="json"` a dict (or a list). Left to the SDK, that `str | dict`
    annotation derives an output schema of `{"result": <either>}` — and the SDK
    then sends the markdown TWICE: once as a text block, once as
    `structuredContent={"result": "<the same markdown>"}`. Clients that prefer
    structured content (Claude Code does) hand the model the second copy: one
    JSON string with every newline escaped. That is the opposite of what the
    markdown default exists for, and it is also not what the spec allows — a
    tool that advertises an outputSchema MUST return structured content
    conforming to it, which a tool whose default output is prose cannot do.

    So no tool advertises an output schema (see `_tool`), and the shape is
    decided here instead:
      * str  -> one text block, real newlines, no structured content;
      * dict -> the JSON as text, plus the dict itself as structured content
                (unwrapped — the `{"result": ...}` envelope is gone);
      * list -> same, but the wire format requires structured content to be an
                object, so a list keeps the `{"result": [...]}` envelope;
      * `_Linked` -> its body shaped as above, then its resource links;
      * anything else (an `Image`) is left for the SDK to convert.
    """
    if isinstance(value, _Linked):
        shaped = _to_result(value.body)
        if isinstance(shaped, CallToolResult):
            shaped.content.extend(value.links)
        return shaped
    if isinstance(value, str):
        return CallToolResult(content=[TextContent(type="text", text=value)])
    if isinstance(value, dict | list):
        # Same serializer, same options, as the SDK's own text rendering, so the
        # json-mode text block is byte-for-byte what clients already received.
        text = pydantic_core.to_json(value, fallback=str, indent=2).decode()
        # Round-tripped so structured content holds only plain JSON types even
        # if a payload ever carries a Path or a datetime.
        plain = json.loads(text)
        structured = plain if isinstance(plain, dict) else {"result": plain}
        return CallToolResult(
            content=[TextContent(type="text", text=text)],
            structured_content=structured,
        )
    return value


_calls_run = 0


def _over_budget() -> CallToolResult | None:
    """The stand-in result once `SEARCH_MCP_TOOL_CALL_BUDGET` is spent.

    A plain result, not an error: a model that sees `isError` tends to retry,
    and the point is to make it stop and write.
    """
    global _calls_run
    budget = settings.tool_call_budget
    if not budget:
        return None
    _calls_run += 1
    if _calls_run <= budget:
        return None
    text = (
        f"Tool budget used up ({budget} call{'s' if budget != 1 else ''}). Do not call "
        "another tool. Answer now from the pages you already have, and list what you "
        "could not confirm."
    )
    return CallToolResult(content=[TextContent(type="text", text=text)])


def _boundary(fn: Callable[..., Any]) -> Callable[..., Any]:
    """Wrap a tool: shape its result, and let anticipated failures reach the
    model with their message.

    `functools.wraps` keeps `__wrapped__`, which is what the SDK's signature
    inspection follows — so the input schema, the `Context` injection and the
    docstring-derived description are all still read off the real function.
    """
    name = fn.__name__
    if inspect.iscoroutinefunction(fn):

        @functools.wraps(fn)
        async def async_wrapper(*args: Any, **kwargs: Any) -> Any:
            refusal = _over_budget()
            if refusal is not None:
                return refusal
            try:
                return _to_result(await fn(*args, **kwargs))
            except Exception as exc:
                raise _translate(name, exc) from exc

        return async_wrapper

    @functools.wraps(fn)
    def sync_wrapper(*args: Any, **kwargs: Any) -> Any:
        refusal = _over_budget()
        if refusal is not None:
            return refusal
        try:
            return _to_result(fn(*args, **kwargs))
        except Exception as exc:
            raise _translate(name, exc) from exc

    return sync_wrapper


# Every tool name this module declares, registered or not. `run()` checks the
# `SEARCH_MCP_TOOLS` allow-list against it so a typo is reported at startup.
_DECLARED_TOOLS: set[str] = set()


def _tool(**kwargs: Any) -> Callable[[Callable[..., Any]], Callable[..., Any]]:
    """`@mcp.tool(...)`, plus the boundary above.

    Registers the WRAPPED function and hands back the original, so
    `from search_mcp.server import search` is still the plain coroutine with
    its native return value and native exceptions.

    `structured_output=False` for every tool: see `_to_result` for why a
    dual-format tool must not advertise a derived output schema.

    `SEARCH_MCP_TOOLS` narrows what gets registered. A tool left out is still
    importable and callable from Python; it is only absent from `tools/list`.
    """
    kwargs.setdefault("structured_output", False)
    server: MCPServer = kwargs.pop("server", None) or mcp

    def register(fn: Callable[..., Any]) -> Callable[..., Any]:
        _DECLARED_TOOLS.add(fn.__name__)
        allowed = settings.enabled_tools()
        if not allowed or fn.__name__ in allowed:
            server.add_tool(_boundary(fn), **kwargs)
        return fn

    return register


@_tool(
    title="Web search (multi-engine, no API key)",
    annotations=ToolAnnotations(
        read_only_hint=True,
        idempotent_hint=False,
        open_world_hint=True,
    ),
)
async def search(
    query: str,
    engines: list[str] | None = None,
    max_results: int = 10,
    use_cache: bool = True,
    max_age_hours: float | None = None,
    freshness: Literal["day", "week", "month", "year"] | None = None,
    include_domains: list[str] | None = None,
    exclude_domains: list[str] | None = None,
    category: Category | None = None,
    include_text: str | None = None,
    exclude_text: str | None = None,
    format: Format = "markdown",
    ctx: Context | None = None,
) -> str | dict[str, Any]:
    """Run a multi-engine web search and return a ranked, deduplicated link list.

    Best for:
    - Discovery queries ("what is X", "find me X", "who is X").
    - Getting a list of URLs you can hand to `fetch` / `fetch_batch` next.
    - Topics likely to be after your knowledge cutoff (use `freshness="week"`).
    - Filtering to specific domains (`include_domains=["python.org"]`) or
      a kind of source (`category="paper"` / `"finance"`, or a sub-group like
      `category="paper.biomed"` / `"finance.filings"`; see `engines()` for the
      full tree).
    - Looking a fact up at its registry, with no category needed: a current
      version or support end date ("latest fastapi version"), a CVE
      ("CVE-2024-3094": NVD record, OSV advisories, CISA exploited status),
      a forecast ("上海明天天气"), an exchange rate ("100 usd to cny": ECB, and
      the PBOC parity for 人民币), a coin price, a country indicator ("china
      gdp", "japan population"), public holidays ("2026年放假安排"), the current
      time in a city, a domain's expiry, a company's legal entity, an iOS
      app's version, Wikidata facts ("Shanghai population"). The source that
      fits the words joins the search and its record arrives as the first
      result with the publisher's date in `dated …`, so it can be cited
      without a fetch. Name the thing plainly. `category=` ("software",
      "security", "weather", "finance.fx", "stats", "calendar", "reference",
      "docs", "gov") asks the same sources explicitly.

    Not recommended for:
    - You already know the URL -> use `fetch` instead.
    - You want both links AND their full text in one call -> use `research`.
    - You want to query pages already in the local cache -> use `cache_search`.
    - Reading PDFs/DOCX from a known URL -> use `read_doc`.
    - Following one paper's references or citations -> use `paper_graph`.

    Returns:
    - markdown (default): numbered list of `n. title`, `<url>`, snippet. About 40%
      fewer tokens than json.
    - json: dict with `results` (list of {title,url,snippet,engines,score}),
      `engines`, `cached`, optional `errors` map, optional `hint` string.
      `engines` is what was actually ASKED for this answer; each result's own
      `engines` is what found it. A default engine that failed recently is not
      asked and appears under `benched_engines` instead; a rescue pass that
      substituted a source appears as `rescued_via`.

    Common mistakes:
    - Answering from snippets. A snippet is an engine's summary of a page as it
      looked when crawled: it drops qualifiers and is often years old. Use
      `search` to find the URL, then read the page (`fetch`, `research`) for
      any date, amount, rule, deadline or number you will state.
    - Treating a result as recent because `freshness=` was set. The filter keeps
      undated results; the header says how many could be dated (`dated: 3/10`)
      and each result is marked `dated …`, `… (from snippet text)` or `undated`.
    - Passing a URL as `query`: that is `fetch`'s job.
    - Cranking `max_results` up hoping for better recall; engines cap around
      10-20 each, anything beyond is duplicate noise (and 50 is the ceiling).
    - Naming engines by default. The default pool is already the set that works
      keylessly over plain HTTP, it benches an engine that starts failing and
      seats a reserve when it gets thin, and naming engines switches all of
      that, and `category=` routing, off. Name engines when you want a
      specific index: `engines=["so360","baidu"]` for Chinese-language sites,
      `engines=["brave"]` or `["startpage"]` (both need the browser) for a
      second opinion.
    - Using `category="news"` for breaking news without also setting
      `freshness="day"`: the index lags by days.

    Args:
        query: Natural-language query (the same string a human would type).
        engines: Subset of `engines()`. None (recommended) = the health-aware
            default pool: duckduckgo, bing, anysearch and mojeek, plus
            googlenews when `freshness` is "day"/"week" and a Chinese index for
            a Chinese query.
        max_results: Merged result count after dedup, clamped to 1-50. It is
            also the PER-ENGINE budget, so it multiplies across the fan-out;
            5-20 is the useful range and anything past that is duplicate noise
            bought with real latency. Omit it for the configured default.
        use_cache: Reuse the last result for this exact (query, engines,
            max_results, AND all active filters: freshness, include/exclude
            domains, category, include/exclude text) within the cache TTL.
            Changing any filter is a different cache entry. False forces a
            re-fetch.
        max_age_hours: Treat cached results older than this as a read miss; a
            fresh result is ALWAYS written back to the cache regardless of this
            value, so caching is never disabled. Use 0 to force-refresh while
            keeping cache writes; None = use server default TTL (7 days).
        freshness: "day"|"week"|"month"|"year". Restricts to recent results.
            Best-effort: applied as an engine time-window param AND a client-side
            date check, but most HTML-engine results carry no parseable date, so
            undated results are kept rather than dropped (unknown != old). Treat
            it as a strong hint, not a hard filter; googlenews dates are exact.
        include_domains: List of domains to restrict to (e.g. ["python.org"]).
        exclude_domains: List of domains to exclude.
        category: Which KIND of source to search. The enum lists every value.
            A bare group widens: "paper" adds one specialist per sub-group to the
            default web pool. A dotted sub-group narrows to just the sources that
            index it ("paper.biomed" => the biomedical indexes only). It also
            RERANKS: engines that natively index the category count double in
            the fusion, so the filing outranks the commentary about it, and a
            record looked up by the query (a PyPI release, a CVE, an ECB rate,
            a forecast) counts five times, so it leads the list. Call
            `engines()` for the group -> sub-group -> engine tree with a line on
            each source. Two behaviours worth knowing: "image"/"dataset" REPLACE
            the web pool rather than augment it (a web engine cannot return an
            image file), and "news"/"paper"/"forum"/"github"/"blog" also filter
            general-web hits by hostname, so a strict category can thin those
            engines out. The specialists it routes to are exempt.
        include_text: Substring required in title or snippet (case-insensitive).
        exclude_text: Substring forbidden in title or snippet.
        format: "markdown" (default) or "json".
    """
    if not query.strip():
        raise ValueError("query must not be empty")

    # aggregate_search owns the single cache key/read/write path. We just hand it
    # the tighter read TTL (max_age_seconds); it tightens the cache READ but
    # ALWAYS writes a fresh non-empty result, so caching is never disabled by a
    # freshness request. This also keeps news-category engine routing inside the
    # one place that computes the key, so the read key can't drift from the
    # write key.
    payload = await aggregate_search(
        query,
        engines=engines,
        max_results=max_results,
        use_cache=use_cache,
        max_age_seconds=_max_age_to_seconds(max_age_hours),
        freshness=freshness,
        include_domains=include_domains,
        exclude_domains=exclude_domains,
        category=category,
        include_text=include_text,
        exclude_text=exclude_text,
    )
    hint = errors_to_hint(payload.get("errors"))
    if hint:
        payload["hint"] = hint
    body = _maybe_render(payload, format, render_search)
    cache_key = payload.get("cache_key")
    if format == "json" and cache_key and _supports_links(ctx):
        # json is what a program asks for, and a program can come back for the
        # same result set by handle instead of re-running the search. Markdown
        # callers are reading, not storing — a link there is just more tokens.
        return _Linked(
            body,
            [
                ResourceLink(
                    type="resource_link",
                    name="cached-search",
                    title="This result set, from the local cache",
                    uri=f"cache://search/{cache_key}",
                    mime_type="application/json",
                )
            ],
        )
    return body


@_tool(
    title="Fetch a URL: page text, document, or resource",
    annotations=ToolAnnotations(
        read_only_hint=True,
        idempotent_hint=True,
        open_world_hint=True,
    ),
    # No derived output schema — true of every tool now (see `_to_result`), and
    # doubly so here: this one can also return an actual image, no single JSON
    # Schema covers an ImageContent block, and from 2026-07-28 the SDK
    # VALIDATES returns against a derived schema — so one here would reject
    # every inline image at call time.
    structured_output=False,
)
async def fetch(
    url: str,
    render: Literal["auto", "http", "browser"] = "auto",
    force_refresh: bool = False,
    max_age_hours: float | None = None,
    inline: bool = False,
    format: Format = "markdown",
    ctx: Context | None = None,
) -> str | dict[str, Any] | Image:
    """Fetch one URL: page text, or a description of a non-text resource.

    Handles any http(s) resource, not just HTML:
    - HTML pages -> reader-mode Markdown (nav/footer/scripts stripped).
    - PDF/DOCX/XLSX/PPTX/EPUB/CSV/code/archives -> parsed text (same engine as
      `read_doc`, which you should prefer when you need pagination).
    - Images, video, audio, fonts, opaque binaries -> a description
      (media type, byte size, dimensions, sha256), NOT the bytes.

    Best for:
    - You already have a URL (from `search`, the user, or your own knowledge)
      and need the actual page text.
    - Verifying a single claim by reading the source.
    - Checking what a resource IS before deciding to spend tokens on it.

    Not recommended for:
    - Multiple URLs at once -> use `fetch_batch` (concurrent, one round-trip).
    - "Search then read top N" -> use `research` (one call, not two).
    - Long documents you need to page through -> use `read_doc` (start/length).
    - You don't have a URL yet -> use `search` first.

    Returns:
    - markdown (default): a small header (URL, render method, token count)
      plus the cleaned page body.
    - json: {url, title, content, method, truncated, tokens_estimated,
      author, published_date, sitename}, plus {media_type, bytes_size, sha256,
      width, height} for non-text resources.
    - With `inline=True` on an image: the image itself, viewable by a
      vision-capable model.

    Common mistakes:
    - Passing a search query instead of a URL.
    - Using `render="http"` on a JS-only SPA: it returns near-empty content;
      use "auto" (default) or "browser".
    - Setting `inline=True` on a large image out of habit. A 1MB image costs
      well over a thousand tokens; fetch it plainly first and inline only if
      the description says it's worth looking at.
    - Forgetting that results are cached 7 days: use `force_refresh=True`
      or `max_age_hours=0` for a fresh pull. The header says `cached N days ago`
      when you are not looking at the live page; for deadlines, prices and
      anything else that moves, that is the cue to refresh.
    - Reading `no publication date found` as "recent". It means unknown.

    Args:
        url: Absolute http(s) URL.
        render: "auto" (try HTTP, fall back to stealth Chromium), "http"
            (fast, fails on JS), "browser" (slow, robust).
        force_refresh: Bypass the page cache entirely.
        max_age_hours: Treat cached pages older than this as a miss. 0 = same
            as force_refresh. None = server default TTL (7 days).
        inline: For images only. Returns the image itself instead of a
            description, so a vision-capable model can see it. Ignored for
            text resources.
        format: "markdown" or "json".
    """
    # max_age_hours=0 means "force refresh"; anything else just tightens the
    # cache-read TTL, which fetch_page applies against the RESOLVED url.
    effective_force = force_refresh or max_age_hours == 0
    max_age_seconds = (
        _max_age_to_seconds(max_age_hours)
        if max_age_hours is not None and max_age_hours > 0
        else None
    )
    result = await fetch_page(
        url,
        render=render,
        force_refresh=effective_force,
        inline=inline,
        max_age_seconds=max_age_seconds,
    )
    if result.data is not None and result.media_type.startswith("image/"):
        # Hand back real MCP image content rather than base64 in a string —
        # the client decides how to show it, and non-vision clients can skip it.
        return Image(data=result.data, format=result.media_type.split("/", 1)[-1])
    payload = result.to_dict()
    body = _maybe_render(payload, format, render_fetch)
    if result.truncated and not result.media_type and _supports_links(ctx):
        # The cache holds the WHOLE body; what was returned is the first
        # `max_content_chars` of it. For an HTML page this link is the only way
        # to the rest — `read_doc` paginates documents, not web pages.
        return _Linked(
            body,
            [
                ResourceLink(
                    type="resource_link",
                    name="full-page",
                    title="Full cached text of this page",
                    uri=f"cache://page/{quote(result.url, safe='')}",
                    description="The untruncated body this result was cut from.",
                    mime_type="text/markdown",
                )
            ],
        )
    return body


@_tool(
    title="Fetch many URLs concurrently",
    annotations=ToolAnnotations(
        read_only_hint=True,
        idempotent_hint=True,
        open_world_hint=True,
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
    - Passing a single URL inside a 1-element list: use `fetch` directly.
    - Assuming an exception means the whole batch failed; check each item's
      `error` field instead.

    Args:
        urls: List of absolute http(s) URLs (max 20 per call).
        render: Same as `fetch`.
        format: "markdown" or "json".
    """
    if not urls:
        # An empty string reads as "all of them failed silently". Say which of
        # the two it is, in the same voice as the >20 branch below.
        return (
            "_No URLs given. Pass 1-20 absolute http(s) URLs._\n"
            if format == "markdown"
            else []
        )
    if len(urls) > 20:
        raise ValueError(
            f"fetch_batch accepts at most 20 URLs per call (got {len(urls)}); "
            "split the list across calls"
        )
    await _safe_progress(ctx, 0.0, float(len(urls)), "starting batch fetch")
    raw = await fetch_many(urls, render=render)
    items: list[dict[str, Any]] = []
    for idx, r in enumerate(raw, 1):
        items.append(r.to_dict() if hasattr(r, "to_dict") else r)
        await _safe_progress(ctx, float(idx), float(len(urls)), f"fetched {idx}/{len(urls)}")
    if format == "json":
        return items
    sections = []
    for it in items:
        if "error" in it:
            sections.append(f"### ⚠ {it.get('url', '')}\n_failed: {it['error']}_\n")
        else:
            sections.append(render_fetch(it))
    return "\n---\n\n".join(sections)


@_tool(
    title="Read a remote (or sandboxed local) document",
    annotations=ToolAnnotations(
        read_only_hint=True,
        idempotent_hint=True,
        # Reads http(s) URLs over the network, so this is an open-world tool.
        open_world_hint=True,
    ),
)
async def read_doc(
    source: str,
    start: int = 0,
    length: int | None = None,
    format: Format = "markdown",
) -> str | dict[str, Any]:
    """Read an http(s) document (or a sandboxed local file) into Markdown.

    Best for:
    - Remote PDFs and DOCX from an http(s) URL (parsed locally, no remote API).
    - Local PDF/DOCX/text/Markdown files, ONLY when local reads are enabled
      (see Security below).
    - Paginating through a long document via `start` / `length`.

    Not recommended for:
    - Arbitrary HTML web pages -> `fetch` does reader-mode cleanup that this
      tool does not.
    - Pages discovered through search -> `fetch` or `research`.

    Security (local files are sandboxed and OFF by default):
    - Local-file reads are DISABLED unless the server operator sets the
      SEARCH_MCP_DOCUMENT_ROOT env var to a directory. With it unset, a local
      path raises a "local file reads are disabled" error. Pass an http(s)
      URL instead, or ask the operator to enable the sandbox.
    - When enabled, `source` must resolve INSIDE that root; relative paths
      resolve against the root (not the process CWD) and any `..` traversal
      that escapes the root is rejected. `file://` URLs are always rejected.
    - Remote http(s) sources are unaffected by this setting.

    Returns:
    - markdown (default): rendered document text with a small header.
    - json: {content, title, format, total_chars, start, returned_chars,
      truncated}. Use `total_chars` and `returned_chars` to drive pagination.

    Common mistakes:
    - Calling this on a normal article URL: you'll get raw HTML noise. Use
      `fetch` instead.
    - Forgetting to advance `start` when paginating: next call should pass
      `start = previous_start + returned_chars`.
    - Passing a negative `length` (raises an error) or a `start` past the end
      (clamped to EOF: you'll get `returned_chars == 0`, `start == total_chars`,
      and `truncated == False`, which is the signal you've paged off the end).

    Args:
        source: http(s) URL, or a local path UNDER SEARCH_MCP_DOCUMENT_ROOT when
            local reads are enabled (disabled by default; see Security).
        start: Character offset to begin reading from. Default 0. Clamped into
            [0, total_chars]; a negative value is treated as 0.
        length: Max characters to return; None = read to end (still capped by
            the per-call max content size). Must be >= 0. A negative length
            is rejected with a ValueError.
        format: "markdown" or "json".
    """
    # Reject a negative `start` at the boundary with a clear, LLM-readable
    # message. documents.py would silently clamp it to 0; surfacing the mistake
    # is more helpful to a calling model than swallowing it. Negative `length`
    # and out-of-range `start` are validated/clamped inside read_document — we
    # do NOT duplicate that logic here (it would risk diverging behavior).
    if start < 0:
        raise ValueError(f"start must be >= 0, got {start}")
    # A blank source is a caller mistake. Passed through, it fell to the
    # local-file branch and came back as "Local file reads are disabled; set
    # SEARCH_MCP_DOCUMENT_ROOT" — an answer to a question nobody asked, and one
    # that sends the caller to configure a sandbox they do not need.
    if not source.strip():
        raise ValueError("source must not be empty: pass an http(s) URL or a file path")
    result = await read_document(source, start=start, length=length)
    payload = result.to_dict()
    return _maybe_render(payload, format, render_doc)


@_tool(
    title="Search and read in one call",
    annotations=ToolAnnotations(
        read_only_hint=True,
        idempotent_hint=False,
        open_world_hint=True,
    ),
)
async def research(
    question: str,
    depth: int = 3,
    engines: list[str] | None = None,
    fetch: bool = True,
    use_cache: bool = True,
    max_age_hours: float | None = None,
    freshness: Literal["day", "week", "month", "year"] | None = None,
    include_domains: list[str] | None = None,
    exclude_domains: list[str] | None = None,
    category: Category | None = None,
    include_text: str | None = None,
    exclude_text: str | None = None,
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
    - Checking or expanding one paper's citations -> `paper_graph`.

    Returns:
    - markdown (default): a "Research brief" with a Sources index then the
      full Markdown body of each fetched document, separated by horizontal
      rules; includes a token estimate.
    - json: {question, engines, sources:[{rank,title,url,snippet,...}],
      documents:[...], tokens_estimated, errors}.

    Common mistakes:
    - Using `depth=8` for a quick lookup: that's 8 page fetches, and 2-3 is
      almost always enough.
    - Calling `research` for a known URL: that is what `fetch` is for.
    - Forgetting that `fetch=False` returns sources only (much cheaper if
      the LLM only needs to pick which one to read).

    Args:
        question: What you want to know, in natural language.
        depth: How many top results to fetch (1-8). 3 is a good default.
        engines: Override the engine set (see `engines()` for names). Prefer
            `category=`. Naming engines turns category routing off.
        fetch: If False, return source list without reading them.
        freshness: "day"|"week"|"month"|"year". Restricts to recent results.
            Best-effort; undated results are kept rather than dropped.
        include_domains: Restrict to these domains (e.g. ["python.org"]).
        exclude_domains: Drop results from these domains.
        category: Which KIND of source to search; a bare group widens, a dotted
            sub-group narrows. Same values as `search`; see `engines()`.
        include_text: Substring required in title or snippet (case-insensitive).
        exclude_text: Substring forbidden in title or snippet.
        use_cache: Reuse cached search/page data within TTL.
        max_age_hours: Treat cached search results AND cached page bodies older
            than this as a read miss; fresh data is always written back. 0 =
            force-refresh both the engine search and every fetched page body;
            None = server default TTL (7 days). A non-zero value is honored for
            both halves (it used to be ignored for anything but 0).
        format: "markdown" or "json".
    """
    await _safe_progress(ctx, 0.05, 1.0, "starting research")

    # max_age_hours tightens the READ TTL for BOTH the search-cache and the
    # page-cache; aggregate_search/_fetch_with_freshness still write fresh data
    # back, so caching is never disabled. max_age_hours=0 force-refreshes both
    # the engine search and every fetched page body.
    max_age_seconds = _max_age_to_seconds(max_age_hours)

    await _safe_progress(ctx, 0.15, 1.0, "searching engines")

    payload = await run_research(
        question,
        depth=depth,
        engines=engines,
        fetch=fetch,
        use_cache=use_cache,
        max_age_seconds=max_age_seconds,
        page_max_age_seconds=max_age_seconds,
        freshness=freshness,
        include_domains=include_domains,
        exclude_domains=exclude_domains,
        category=category,
        include_text=include_text,
        exclude_text=exclude_text,
    )

    # Coarse end-of-fetch milestones — research.py runs fetch_many internally
    # so we can't checkpoint per-URL without rewriting it.
    n_docs = max(1, len(payload.get("documents") or [1]))
    await _safe_progress(ctx, 0.95, 1.0, f"fetched {n_docs} sources")
    await _safe_progress(ctx, 1.0, 1.0, "done")

    return _maybe_render(payload, format, render_research)


@_tool(
    title="Walk a paper's citation graph",
    annotations=ToolAnnotations(
        read_only_hint=True,
        idempotent_hint=True,
        open_world_hint=True,
    ),
)
async def paper_graph(
    paper: str,
    direction: Literal["both", "references", "citations"] = "both",
    limit: int = 10,
    format: Format = "markdown",
) -> str | dict[str, Any]:
    """Follow the citations of ONE paper, and check whether it still stands.

    `search` finds papers that MENTION your words. This follows the edges
    instead: what a specific paper built on, and what has built on it since.

    Best for:
    - Checking a citation before repeating it: is the DOI real, and has the
      paper been retracted or corrected?
    - "What happened after this result": citing works come back ordered by how
      much the field cited them, so a 2019 paper leads to the current state of
      the art rather than to the most recent preprint about it.
    - Building a reading list backwards from one good paper.

    Not recommended for:
    - Finding papers by topic -> `search(category="paper")`, or a sub-group
      like `"paper.biomed"` / `"paper.cs"` / `"paper.preprint"`.
    - Reading the paper itself -> `read_doc` on the returned URL.

    Returns:
    - markdown (default): the paper with its retraction/correction notices,
      then "References" and "Cited by" sections.
    - json: {paper, references, citations, notes}, where `paper.crossref`
      carries `registered` and every post-publication `notices` entry.

    Common mistakes:
    - Passing a topic instead of a paper. A title resolves to its single best
      match; a phrase that names no specific paper resolves to the wrong one.
    - Reading an empty `citations` list as "uncited" when `notes` says the
      lookup was truncated.

    Args:
        paper: DOI (`10.1145/1571941.1572114`, or a doi.org URL), an OpenAlex
            ID (`W2148972377`), or the paper's exact title.
        direction: "both", "references" (what it cites) or "citations" (what
            cites it).
        limit: Max neighbours per direction, 1-50.
        format: "markdown" or "json".
    """
    if not paper.strip():
        raise ValueError(
            "paper must not be empty: pass a DOI, an OpenAlex ID, or the exact title"
        )
    payload = await run_paper_graph(paper, direction=direction, limit=limit)
    return _maybe_render(payload, format, render_paper_graph)


@_tool(
    title="Search local cache (FTS5)",
    annotations=ToolAnnotations(
        read_only_hint=True,
        idempotent_hint=True,
        open_world_hint=False,
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
    - json: list of {url, title, snippet, author, date, sitename}. The last
      three are "" when the cached row predates metadata capture.

    Common mistakes:
    - Treating this like web search: it ONLY hits pages already in the local
      cache. If the user hasn't fetched anything, you'll get zero hits.
    - Using natural-language phrases without quoting them; FTS5 splits on
      whitespace as AND. For an exact phrase use `"like this"`.

    Args:
        query: FTS5 query. Bare terms = AND. Supports OR / NOT, prefix
            (`term*`), and phrase (`"exact phrase"`).
        limit: Max hits to return.
        format: "markdown" or "json".
    """
    # The cache stores metadata packed behind a sentinel in the title column,
    # so raw rows would hand the caller "\x01META\x01{...}" as a page title.
    if not query.strip():
        # Distinct from "the cache has nothing": telling a caller to populate a
        # cache that may already be full sends them to fix the wrong thing.
        if format == "json":
            return []
        return "_Empty query. Pass FTS5 terms, e.g. `asyncio` or `\"exact phrase\"`._\n"
    rows = [decode_cached_title(r) for r in await cache.search_pages(query, limit=limit)]
    if format == "json":
        return rows
    if not rows:
        bad = _invalid_fts_hint(query)
        if bad:
            return (
                f"_No results: your search syntax looks invalid. {bad}_\n"
            )
        return (
            f"_No cached pages match `{query}`. "
            "Use `fetch` or `research` to populate the cache._\n"
        )
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


@_tool(
    title="List available search engines",
    annotations=ToolAnnotations(
        read_only_hint=True,
        idempotent_hint=True,
        open_world_hint=False,
    ),
)
def engines(
    group: EngineGroup | Category | None = None,
    format: Format = "markdown",
) -> str | dict[str, Any]:
    """List the available sources, grouped by what they index.

    Best for:
    - Choosing a source deliberately: which one indexes filings, or preprints,
      or Chinese-language pages.
    - Checking a name before passing it to `engines=` on `search` / `research`.

    Not recommended for:
    - Calling on every search: the list is static, so read it once.

    Returns (markdown): a `group -> sub-group -> engine` tree, one line of
    description per engine. `group="paper"` restricts it to that group.
    Returns (json): `{"engines": [...names...], "taxonomy": {...},
    "descriptions": {...}}`.

    Prefer `category=` over `engines=`. `category="paper"` WIDENS: it routes to
    one specialist per sub-group. A dotted sub-group NARROWS: `"paper.biomed"`
    queries only the biomedical indexes. Naming engines explicitly turns that
    routing off entirely, so reach for it only to force a specific source.

    Common mistakes:
    - Passing one of these names as `query`: they belong in `engines=`.
    - Passing a key-only engine with no key configured; it returns an
      actionable error, not results.
    """
    taxonomy = source_taxonomy()
    if group:
        key = group.split(".", 1)[0].strip().lower()
        if key not in taxonomy:
            # `pdf` and `blog` are valid `category=` values with no source group
            # behind them. An empty tree would read as "nothing is installed".
            raise ValueError(
                f"{group!r} is a result filter, not a source group: no engine indexes it "
                f"natively, so `category={group!r}` filters the default engines' results. "
                f"Source groups: {', '.join(taxonomy)}."
            )
        taxonomy = {key: taxonomy[key]}
    names = [n for subs in taxonomy.values() for names_ in subs.values() for n in names_]
    descriptions = {n: ENGINES[n].description for n in dict.fromkeys(names)}
    # Opt-in engines cannot run without the operator's own key. Derived from the
    # keystore registry the settings page already drives, so the two can never
    # disagree — and it includes `github_code`, which the old
    # `not provider.optional` test missed because its provider IS optional.
    opt_in = {name: on for name, on in opt_in_engines().items() if name in descriptions}
    # Asked the same question category routing asks, so the answer is the same.
    unrouted = {
        name for name in descriptions if name not in opt_in and not ENGINES[name].is_available()
    }
    if format == "json":
        return {
            "engines": list_engines() if not group else list(descriptions),
            "taxonomy": taxonomy,
            "descriptions": descriptions,
            "needs_api_key": sorted(opt_in),
            "opt_in": opt_in,
            "not_auto_routed": sorted(unrouted),
        }
    return render_engines(taxonomy, descriptions, opt_in, unrouted)


@_tool(
    title="Compare URLs side-by-side",
    annotations=ToolAnnotations(
        read_only_hint=True,
        idempotent_hint=True,
        open_world_hint=True,
    ),
)
async def compare(
    question: str,
    urls: list[str],
    format: Format = "markdown",
) -> str | dict[str, Any]:
    """Fetch 2-5 URLs concurrently and return per-URL excerpts so the LLM can
    compare them against a single question in one round trip.

    Best for:
    - Side-by-side product/feature/article comparisons.
    - "Compare X to Y" or "How does A differ from B" queries.
    - Triangulating a fact across multiple sources.

    Not recommended for:
    - >5 URLs -> use `fetch_batch`.
    - 1 URL -> use `fetch`.
    - Don't have URLs yet -> use `search` or `research` first.

    Returns:
    - markdown (default): a comparison brief with per-URL sections, each
      containing title, sitename, published date, and a smart-truncated excerpt.
    - json: {question, urls, excerpts:[{url, title, excerpt, ...}],
      tokens_estimated}.

    Common mistakes:
    - Asking `compare` to actually answer the question: it returns material,
      the LLM does the comparison.
    - Passing >5 URLs and expecting them all to fit in context: use
      `fetch_batch` for bulk reads.

    Args:
        question: The comparison question the LLM will answer using the
            returned excerpts.
        urls: 2-5 absolute http(s) URLs.
        format: "markdown" (default) or "json".
    """
    payload = await compare_urls(question, urls)
    return _maybe_render(payload, format, render_compare)


@_tool(
    title="Extract structured data from a URL",
    annotations=ToolAnnotations(
        read_only_hint=True,
        idempotent_hint=True,
        open_world_hint=True,
    ),
)
async def extract_structured(
    url: str,
    format: Format = "markdown",
) -> str | dict[str, Any]:
    """Pull JSON-LD, OpenGraph, Twitter cards, and microdata from a web page.

    Best for:
    - Product pages (price, currency, availability, brand, rating).
    - Article pages (author, publish date, image, headline).
    - Recipe / event / video pages where rich metadata IS the answer.
    - Cases where `fetch` returns prose but you need fields.

    Not recommended for:
    - Just reading a page -> use `fetch`.
    - PDFs / DOCX -> use `read_doc`.
    - Pages that don't publish schema.org metadata (most blogs): you'll get
      empty lists; fall back to `fetch`.

    Returns:
    - json: {url, json_ld:[], microdata:[], opengraph:[], rdfa:[]}. Twitter
      card meta tags are surfaced inside the `opengraph` list.
    - markdown (default): a flattened key/value view with each block printed
      as a JSON code block under its syntax heading.

    Common mistakes:
    - Calling on every URL "just in case": most sites have no structured
      data, and `fetch` is what you actually want.

    Args:
        url: Absolute http(s) URL.
        format: "markdown" (default) or "json".
    """
    payload = await _extract_structured(url)
    return _maybe_render(payload, format, render_structured)


@_tool(
    title="Download a file to disk",
    annotations=ToolAnnotations(
        # The one tool here that creates a caller-visible local file.
        read_only_hint=False,
        idempotent_hint=True,
        open_world_hint=True,
    ),
)
async def download(
    url: str,
    format: Format = "markdown",
) -> str | dict[str, Any]:
    """Save a file from a URL to a local, auto-expiring download directory.

    Downloads are enabled by default and saved under
    `SEARCH_MCP_CACHE_DIR/downloads`. Set `SEARCH_MCP_DOWNLOAD_ENABLED=false`
    to disable them or `SEARCH_MCP_DOWNLOAD_DIR` to override the destination.

    Best for:
    - Keeping an actual file (installer, dataset, archive, image) rather than
      its text.
    - Handing a path to another tool that needs a real file on disk.

    Not recommended for:
    - Reading a document's contents -> use `read_doc`, which parses it without
      touching the filesystem.
    - Looking at a web page -> use `fetch`.
    - Viewing an image -> use `fetch(inline=True)`.

    Returns:
    - markdown (default): where the file was saved, its size and type.
    - json: {url, saved_path, media_type, bytes_size, sha256, expires_in_hours}.
      An expires_in_hours value of 0 means TTL cleanup is disabled.

    Retention: files older than SEARCH_MCP_DOWNLOAD_TTL_HOURS (default 24) are
    deleted before the next download and at startup. A value of 0 disables TTL
    cleanup. Otherwise, treat the path as short-lived and copy it elsewhere if
    you need to keep it.

    Args:
        url: Absolute http(s) URL of the file to save.
        format: "markdown" or "json".
    """
    downloads.require_download_dir()

    import anyio

    # Directory scans and writes are blocking syscalls; keep them off the event
    # loop so a large download directory or save cannot stall other requests.
    await anyio.to_thread.run_sync(downloads.purge_expired)
    result = await fetch_bytes(url)
    path = await anyio.to_thread.run_sync(
        downloads.save, url, result.data or b"", result.media_type
    )

    payload = {
        "url": url,
        "saved_path": str(path),
        "media_type": result.media_type,
        "bytes_size": result.bytes_size,
        "sha256": result.sha256,
        "expires_in_hours": settings.download_ttl_hours,
    }
    if result.width is not None:
        payload["width"] = result.width
        payload["height"] = result.height
    if format == "json":
        return payload
    lines = [
        f"# Downloaded `{path.name}`",
        "",
        f"- **Saved to:** `{path}`",
        f"- **Type:** {result.media_type or 'unknown'}",
        f"- **Size:** {result.bytes_size:,} bytes",
    ]
    if result.width is not None:
        lines.append(f"- **Dimensions:** {result.width}×{result.height}px")
    lines.append(f"- **SHA-256:** `{result.sha256}`")
    lines.append("")
    if settings.download_ttl_hours == 0:
        lines.append("> TTL cleanup is disabled; this file will not expire automatically.")
    else:
        lines.append(
            f"> Deleted automatically after {settings.download_ttl_hours}h. "
            "Copy it elsewhere to keep it."
        )
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# The optional answer agent (agent.py)
# ---------------------------------------------------------------------------


async def ask(
    question: str,
    freshness: Literal["day", "week", "month", "year"] | None = None,
    include_domains: list[str] | None = None,
    category: Category | None = None,
    format: Format = "markdown",
    ctx: Context | None = None,
) -> str | dict[str, Any]:
    """Delegate one web question and get back a short answer with dated sources.

    The server searches, reads the top pages, and has the language model its
    operator configured answer from them. You receive a few sentences and the
    source URLs. The page text stays out of your context.

    Best for:
    - A quick factual lookup where you want the answer and its sources, and do
      not need the pages ("latest stable version of X", "when does Y close").
    - Keeping your own context small during a long task.

    Not recommended for:
    - Facts someone will act on (deadlines, prices, rules): read the primary
      page yourself with `research` or `fetch`. The answering model is small.
    - Reading a URL you already have -> `fetch`.
    - Anything that needs more than a paragraph -> `research`.

    Returns:
    - markdown (default): the answer, a line saying which model produced it and
      how long it took, and the pages that were read.
    - json: {question, answer, backend, model, sources:[{rank,title,url,date}],
      tool_calls, model_calls, usage, elapsed_seconds, retrieved_at}.
    - When the model fails, `answer` is null and the pages come back as a
      `research` brief, so answer from those.

    Common mistakes:
    - Packing several questions into one call. Ask one thing per call.
    - Quoting the answer without its sources. Pass the URLs and dates on.
    - Treating the answer as verified. It is one small model's reading of a
      few pages.

    Args:
        question: One question, in natural language, in the user's language.
        freshness: "day"|"week"|"month"|"year" for questions about recent events.
        include_domains: Restrict the search to these domains when you know the
            official site.
        category: Which kind of source to search. Same values as `search`.
        format: "markdown" or "json".
    """
    await _safe_progress(ctx, 0.1, 1.0, "searching and reading")
    payload = await run_ask(
        question,
        freshness=freshness,
        include_domains=include_domains,
        category=category,
    )
    await _safe_progress(ctx, 1.0, 1.0, "done")
    return _maybe_render(payload, format, render_ask)


def enable_ask(server: MCPServer | None = None) -> None:
    """Register `ask`. Called at import only when an answer backend is set, so a
    default install lists eleven tools and pays nothing for this one."""
    _tool(
        server=server,
        title="Ask the web, get a short sourced answer",
        annotations=ToolAnnotations(
            read_only_hint=True,
            idempotent_hint=False,
            open_world_hint=True,
        ),
    )(ask)


# Declared whether or not it is registered, so that naming it in
# SEARCH_MCP_TOOLS is never reported as a typo.
_DECLARED_TOOLS.add("ask")

if settings.agent_backend != "off":
    enable_ask()
    _problem = config_problem()
    if _problem:
        # Registered anyway: the same text is what a call returns, and an
        # operator reading the log sees it before any model does.
        log.warning("answer agent is misconfigured: %s", _problem)


# ---------------------------------------------------------------------------
# Prompts (slash-commands in MCP clients)
# ---------------------------------------------------------------------------


@mcp.prompt(title="Research thoroughly")
def research_prompt(question: str, depth: int = 3, category: str = "") -> str:
    """Instruct the model to do a thorough, cited research pass on a question."""
    scope = f", category={category!r}" if category else ""
    return (
        f"You have access to the search-mcp tools. Research the following "
        f"question thoroughly and produce a well-cited answer.\n\n"
        f"QUESTION: {question}\n\n"
        f"PROCEDURE:\n"
        f"1. Call the `research` tool with question={question!r} and depth={depth}{scope}.\n"
        f"2. Read each fetched source. Note each one's publish date and whether the "
        f"copy was cached (both are shown); treat an undated source as of unknown "
        f"age, not as current. If a source seems unreliable or stale, call `search` "
        f"for a corroborating source.\n"
        f"3. If any document was truncated, call `fetch` again with that URL "
        f"or use `read_doc` for paginating PDFs.\n"
        f"4. Write a synthesis (3-8 paragraphs) that:\n"
        f"   - Answers the question directly in the first sentence.\n"
        f"   - Cites sources inline using [1], [2], ... markers that match the\n"
        f"     order returned by `research`.\n"
        f"   - Notes any disagreement between sources.\n"
        f"   - Lists the full source URLs at the end under a 'Sources' header.\n"
        f"   - Takes facts from the fetched pages, never from search snippets.\n"
        f"5. If you could not find a confident answer, say so explicitly and\n"
        f"   show what was checked. End with a 'Could not verify' list of any\n"
        f"   requested detail you did not find on a page you actually read."
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
        f"   Judge from the pages, not from snippets, and check each page's\n"
        f"   publish date: a true statement about last year's edition is a false\n"
        f"   one about this year's.\n"
        f"3. For each source, quote the supporting or contradicting passage.\n"
        f"4. Output a verdict on a 5-point scale: TRUE / MOSTLY TRUE / MIXED /\n"
        f"   MOSTLY FALSE / FALSE, followed by a one-paragraph justification\n"
        f"   with [n]-style citations matching the source order.\n"
        f"5. End with a 'Sources' list of URLs.\n"
        f"6. If sources disagree, surface that explicitly rather than picking\n"
        f"   one side silently."
    )


@mcp.prompt(title="Compare sources")
def compare_sources(question: str, urls: str) -> str:
    """Instruct the model to use `compare` against several URLs and answer
    the question with per-URL citations."""
    return (
        f"Use the `compare` tool with question={question!r} and "
        f"urls={urls!r} (comma-separated). For each excerpt returned, "
        "answer the question with [n] citations to the URL it came from. "
        "If the excerpts disagree, surface that explicitly rather than "
        "picking one side silently."
    )


@mcp.prompt(title="News brief")
def news_brief(topic: str, since: str = "day") -> str:
    """Instruct the model to produce a fresh news brief using `search` +
    `fetch_batch`, with citations."""
    return (
        f"Use the `search` tool with query={topic!r}, category='news', "
        f"freshness={since!r}. Then fetch the top 3 results in parallel "
        "via `fetch_batch`. Produce a 5-bullet brief, with [n] citations "
        "matching the order returned by `search`, each bullet carrying the "
        "story's publish date. Drop anything you cannot date. End with a "
        "'Sources' list of URLs."
    )


@mcp.prompt(title="Quick search")
def quick_search(question: str) -> str:
    """A fast, sourced lookup: the quick-search agent's instructions plus the
    question. Run it inline, or hand the text to a subagent on a host that has
    no agent files (Codex's `spawn_agent` takes it as the message)."""
    return f"{HOST_AGENT_PROMPT}\n\nQuestion: {question}"


# ---------------------------------------------------------------------------
# Resource templates — expose cached data as readable resources
# ---------------------------------------------------------------------------


@mcp.resource("cache://page/{url}", title="Cached page")
async def cached_page(url: str) -> str:
    """Return the cached Markdown body for a previously-fetched URL.

    The URL must be percent-encoded when embedded in the resource URI
    (RFC 6570 templates do not allow `:` or `/` inside variable expansions).
    """
    # The SDK has ALREADY percent-decoded the template variable. Decoding again
    # turned every cached URL that legitimately contains an escape — any
    # non-ASCII Wikipedia title, any `?q=a%20b` — into a different string, and
    # a guaranteed miss. So: the value as given first; one more decode only as a
    # fallback, for a client that double-encoded.
    page = await cache.get_page(url)
    if not page and "%" in url:
        page = await cache.get_page(unquote(url))
    if not page:
        raise ResourceNotFoundError(f"Not in cache: {url}")
    return page.get("content") or ""


@mcp.resource("cache://search/{query_hash}", title="Cached search result")
async def cached_search(query_hash: str) -> str:
    """Return the cached merged result list for a search query hash.

    The hash is the same one the aggregator uses internally to key the
    `search_cache` table. Useful for exposing prior `search` invocations
    as MCP resources without re-running them.
    """
    hit = await cache.get_search(query_hash)
    if hit is None:
        raise ResourceNotFoundError(f"No cached search for hash: {query_hash}")
    # get_search also returns the provenance recorded with the row; this
    # resource is defined as the merged RESULT LIST, so only that half is
    # serialised. Changing the shape here would break every existing reader.
    rows, _meta = hit
    import json
    return json.dumps(rows, ensure_ascii=False, indent=2)


# ---------------------------------------------------------------------------
# Completion — for prompt arguments and resource-template variables
# ---------------------------------------------------------------------------
#
# The protocol offers completion for exactly those two reference kinds. Tool
# arguments are not completable; theirs is the JSON-Schema enum (`Category`,
# `Freshness`, `EngineGroup`), which clients already render as a picker.

_PROMPT_CHOICES: dict[tuple[str, str], tuple[str, ...]] = {
    ("research_prompt", "category"): get_args(Category),
    ("research_prompt", "depth"): tuple(str(n) for n in range(1, 9)),
    ("news_brief", "since"): get_args(Freshness),
}
_COMPLETION_LIMIT = 50


@mcp.completion()
async def _complete(ref: Any, argument: Any, context: Any) -> Completion | None:
    typed = argument.value or ""
    if isinstance(ref, PromptReference):
        choices = _PROMPT_CHOICES.get((ref.name, argument.name))
        if choices is None:
            return None
        return Completion(values=[c for c in choices if c.startswith(typed)][:_COMPLETION_LIMIT])
    if isinstance(ref, ResourceTemplateReference):
        if ref.uri == "cache://page/{url}" and argument.name == "url":
            # What was typed may be raw ("python.org") or already encoded
            # ("https%3A%2F%2Fdocs"). Raw first: a cached URL can itself contain
            # escapes, and decoding a fragment of one would stop it matching.
            urls = await cache.complete_page_urls(typed, _COMPLETION_LIMIT)
            if not urls and "%" in typed:
                urls = await cache.complete_page_urls(unquote(typed), _COMPLETION_LIMIT)
            # Percent-encoded, because that is the only form the template
            # accepts: a raw `https://…` cannot match `{url}` (RFC 6570 simple
            # expansion stops at `/` and `:`).
            return Completion(values=[quote(u, safe="") for u in urls])
        if ref.uri == "cache://search/{query_hash}" and argument.name == "query_hash":
            return Completion(values=await cache.recent_search_keys(typed, _COMPLETION_LIMIT))
    return None


_LOOPBACK_HOSTS = ("127.0.0.1", "localhost", "[::1]")


def _http_security(host: str, port: int) -> TransportSecuritySettings:
    """Build the DNS-rebinding guard for the HTTP transport.

    Passing no settings at all leaves the guard OFF (the SDK defaults that way
    for backwards compatibility), which would let any web page a user visits
    drive this server through their browser. So it is always constructed —
    every host/origin that can legitimately reach the bind address is
    enumerated instead.
    """
    hosts = [f"{h}:{port}" for h in _LOOPBACK_HOSTS]
    origins = [f"http://{h}:{port}" for h in _LOOPBACK_HOSTS]
    if host not in (*_LOOPBACK_HOSTS, "0.0.0.0", "::"):  # noqa: S104 - comparison, not a bind
        hosts.append(f"{host}:{port}")
        origins.append(f"http://{host}:{port}")
    extra = [o for o in settings.http_allowed_origins.replace(",", " ").split() if o]
    origins.extend(extra)
    return TransportSecuritySettings(
        enable_dns_rebinding_protection=True,
        allowed_hosts=hosts,
        allowed_origins=origins,
    )


def run(
    transport: str | None = None,
    host: str | None = None,
    port: int | None = None,
    path: str | None = None,
) -> None:
    """Start the MCP server. Arguments override the SEARCH_MCP_* settings."""
    transport = transport or settings.transport
    allowed = settings.enabled_tools()
    unknown = allowed - _DECLARED_TOOLS
    if unknown:
        log.warning(
            "SEARCH_MCP_TOOLS names no such tool: %s. The tools are: %s",
            ", ".join(sorted(unknown)),
            ", ".join(sorted(_DECLARED_TOOLS)),
        )
    # The two settings that decide whether `ask` exists, disagreeing.
    if allowed and "ask" not in allowed and settings.agent_backend != "off":
        log.warning(
            "SEARCH_MCP_AGENT_BACKEND=%s is set, but SEARCH_MCP_TOOLS leaves `ask` out, "
            "so the tool is not registered",
            settings.agent_backend,
        )
    if "ask" in allowed and settings.agent_backend == "off":
        log.warning("SEARCH_MCP_TOOLS names `ask`, which also needs SEARCH_MCP_AGENT_BACKEND")
    if settings.tool_call_budget and transport == "streamable-http":
        log.warning(
            "SEARCH_MCP_TOOL_CALL_BUDGET=%s on a long-lived HTTP server: after that many "
            "calls every tool answers 'budget used up' until the process restarts",
            settings.tool_call_budget,
        )
    # Ephemeral downloads are swept here as well as before each download:
    # this process is often short-lived, so a background timer would
    # frequently never fire.
    downloads.purge_expired()
    try:
        if transport == "streamable-http":
            host = host or settings.http_host
            port = port if port is not None else settings.http_port
            path = path or settings.http_path
            log.info("search-mcp listening on http://%s:%s%s", host, port, path)
            mcp.run(
                transport="streamable-http",
                host=host,
                port=port,
                streamable_http_path=path,
                transport_security=_http_security(host, port),
            )
        else:
            mcp.run()
    finally:
        try:
            import anyio
            anyio.run(pool.shutdown)
        except Exception:
            pass
        try:
            import anyio
            # A clean close checkpoints the WAL; without it the daemon-thread
            # workaround in cache.py is the only thing standing between us and
            # a hung exit.
            anyio.run(cache.close)
        except Exception:
            pass


__all__ = ["mcp", "run"]
