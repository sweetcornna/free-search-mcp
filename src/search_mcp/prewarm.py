"""Start the slow first-use pieces while the client is still getting ready.

The first search of a process used to pay for two things no later search pays
for. Measured 2026-10-22: Bing's warm-up GET (1.6 s, once per half hour) and
the browser start (2 to 3 s, once per process) when an engine in the pool
needed a render. The server starts this when it starts; a search that arrives
first simply waits on the same jar mint or browser launch, which it would have
done anyway.

The browser is launched only when there is evidence a plain search will need
it: an engine in the pool always renders, or one's HTTP path was walled within
the last day. Starting Chromium for every server would cost memory to the many
that never render a page.

The server module no longer imports the search and fetch code, so the
handshake does not wait for it. This loads it in a worker thread just after
startup instead, so the first search does not wait for it either. Measured
2026-10-22 in a freshly installed environment: 10 s of the old 12 s start was
those imports, most of it the first load of lxml, Pillow, rapidfuzz and the
other native extensions.
"""

from __future__ import annotations

import asyncio
import importlib
import logging

from .browser import pool
from .config import settings
from .health import http_walls

log = logging.getLogger(__name__)

# What nearly every session uses within its first calls: search (which loads
# the engine registry) and fetch (trafilatura). Documents, structured data and
# the answer agent load when first asked for.
_SEARCH_PATH = ("search_mcp.aggregator", "search_mcp.fetcher")


def _import_search_path() -> None:
    for name in _SEARCH_PATH:
        importlib.import_module(name)


def _wants_browser(names: list[str]) -> bool:
    from .engines import get_engine

    if settings.fetch_strategy == "http" or pool.known_unavailable:
        return False
    if settings.fetch_strategy == "browser":
        return True
    for name in names:
        try:
            engine = get_engine(name)
        except ValueError:
            continue
        if engine.needs_browser:
            return True
        if engine.supports_browser_fallback and http_walls.recently_walled(name):
            return True
    return False


async def prewarm() -> None:
    """Warm what the next search will need. Never raises."""
    if not settings.prewarm:
        return
    try:
        await asyncio.to_thread(_import_search_path)
        from .aggregator import agent_lane_now, default_pool_now
        from .engines import get_engine

        names = default_pool_now()
        jobs = []
        if "bing" in names and settings.fetch_strategy != "browser":
            jobs.append(get_engine("bing").warm())
        if _wants_browser(names):
            jobs.append(pool.launch())
        # A signed-in agent engine's first search would otherwise refresh its
        # token and look its model up before searching.
        jobs.extend(get_engine(name).warm() for name in agent_lane_now())
        for outcome in await asyncio.gather(*jobs, return_exceptions=True):
            if isinstance(outcome, Exception):
                log.debug("prewarm step failed: %s", outcome)
    except Exception as e:
        log.debug("prewarm failed: %s", e)


__all__ = ["prewarm"]
