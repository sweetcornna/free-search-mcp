"""SearXNG meta-search aggregator scraper.

Public SearXNG instances proxy results from many backends (Google, Bing,
DDG, Wikipedia, ...) and render them on a single HTML page. Most public
instances disable the JSON output to discourage abuse, but the regular
HTML response is stable across versions and parses reliably.

Strategy:
  * Try a shortlist of known-good public instances in random order.
  * RACE a small batch concurrently: the first instance in the batch to
    return >0 parsed results wins, and the rest are cancelled. Dead/slow
    instances therefore cost ~one per-instance timeout for the whole batch
    instead of K x timeout serially.
  * Use ``curl_cffi`` with the Chrome JA3 fingerprint — vanilla httpx
    triggers the 429/403 anti-bot pages on most instances.
"""

from __future__ import annotations

import asyncio
import random
from typing import Any
from urllib.parse import quote_plus, urljoin

from curl_cffi.requests import AsyncSession
from curl_cffi.requests.exceptions import RequestException

from ..config import settings
from ..net import curl_proxy_kwargs
from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    apply_post_filters,
    apply_post_filters_with_diagnostics,
    augment_query_with_operators,
    extract_date_hint,
    parse_html,
    text_of,
)


# Pinned at chrome131 to match the rest of the project. Using a fingerprint
# Searx instances see all day from real browsers avoids the 403/429 wall
# vanilla httpx hits.
_IMPERSONATE = "chrome131"

# Per-instance timeout. Kept short on purpose: if one instance is slow we
# want to fall through to the next quickly rather than blow the latency
# budget the new defaults are trying to defend.
_PER_INSTANCE_TIMEOUT = 5.0

# How many instances to race in parallel per batch. Racing a small batch
# bounds worst-case latency at ~one timeout per batch instead of one timeout
# per instance, while not hammering every public instance at once on the
# common case where the first batch already returns results.
_RACE_BATCH = 3

# Public instances verified live (Apr 2026) to return parseable results
# under a Chrome-impersonated curl_cffi session. Order is randomised at
# request time to spread load and avoid pinning a single instance.
_INSTANCES: list[str] = [
    "https://search.inetol.net",
    "https://baresearch.org",
    "https://searx.tiekoetter.com",
    "https://opnxng.com",
    "https://search.rhscz.eu",
]

# SearXNG <-> our freshness vocabulary.
_SEARX_FRESHNESS = {"day": "day", "week": "week", "month": "month", "year": "year"}


class SearxEngine(Engine):
    """Meta-search via public SearXNG instances. No API key, no browser."""

    name = "searx"
    needs_browser = False

    # Kept so the abstract contract is satisfied and so callers that compute
    # a cache key from build_url() get a stable string. The actual fetch in
    # search() may use a different instance after fallbacks.
    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        return self._instance_url(_INSTANCES[0], query, filters)

    def _instance_url(
        self, instance: str, query: str, filters: SearchFilters | None
    ) -> str:
        filetype = None
        if filters and filters.category == "pdf":
            filetype = "pdf"
        q = augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
            filetype=filetype,
        )
        url = urljoin(instance + "/", f"search?q={quote_plus(q)}")
        if filters and filters.freshness:
            url += f"&time_range={_SEARX_FRESHNESS[filters.freshness]}"
        return url

    def parse(self, html: str) -> list[SearchResult]:
        tree = parse_html(html)
        results: list[SearchResult] = []
        for art in tree.css("article.result"):
            classes = art.attributes.get("class") or ""
            # SearXNG marks ad-style entries; current public instances rarely
            # show them, but we skip defensively.
            if "result-ad" in classes:
                continue
            link = art.css_first("h3 a")
            if not link:
                # Some templates put the canonical link in .url_header instead.
                link = art.css_first("a.url_header")
            if not link:
                continue
            url = link.attributes.get("href", "") or ""
            title = text_of(link)
            snippet = text_of(art.css_first("p.content"))
            if not url or not title:
                continue
            result = SearchResult(
                title=title, url=url, snippet=snippet, engine=self.name, rank=0
            )
            hint = extract_date_hint(snippet) or extract_date_hint(title)
            if hint:
                result.published_age = hint
            results.append(result)
        return results

    async def _fetch_instance(
        self, instance: str, query: str, filters: SearchFilters | None
    ) -> list[SearchResult]:
        """Fetch + parse a single instance. Never raises: a flaky instance must
        not poison the engine, so every failure path returns ``[]``."""
        url = self._instance_url(instance, query, filters)
        try:
            async with AsyncSession(
                impersonate=_IMPERSONATE,
                timeout=_PER_INSTANCE_TIMEOUT,
                allow_redirects=True,
                headers={
                    "Accept-Language": settings.accept_language,
                    "Accept": (
                        "text/html,application/xhtml+xml,"
                        "application/xml;q=0.9,*/*;q=0.8"
                    ),
                },
                **curl_proxy_kwargs("searx"),
            ) as client:
                resp = await client.get(url)
                if resp.status_code != 200:
                    return []
                return self.parse(resp.text)
        except asyncio.CancelledError:
            # Lost the race — propagate so the gather/cancel path unwinds cleanly.
            raise
        except (RequestException, asyncio.TimeoutError):
            return []
        except Exception:
            return []

    async def search(
        self,
        query: str,
        max_results: int,
        filters: SearchFilters | None = None,
        diagnostics: dict[str, Any] | None = None,
    ) -> list[SearchResult]:
        # Randomise instance order: spreads load + avoids pinning one instance
        # for the whole process. We then RACE a small batch concurrently so a
        # dead/slow instance costs ~one timeout for the batch instead of one
        # timeout serially per instance.
        order = list(_INSTANCES)
        random.shuffle(order)

        results: list[SearchResult] = []
        for start in range(0, len(order), _RACE_BATCH):
            batch = order[start : start + _RACE_BATCH]
            tasks = [
                asyncio.ensure_future(self._fetch_instance(inst, query, filters))
                for inst in batch
            ]
            try:
                # Take the FIRST task that yields a non-empty result; cancel the
                # rest. Empty/failed tasks are skipped, so a fast-but-dead
                # instance can't beat a slower-but-live one.
                for fut in asyncio.as_completed(tasks):
                    try:
                        parsed = await fut
                    except Exception:
                        # Never-raise contract: ignore individual task failures.
                        continue
                    if parsed:
                        results = parsed
                        break
            finally:
                for t in tasks:
                    if not t.done():
                        t.cancel()
                # Drain cancellations so no "task was destroyed but pending"
                # warnings leak; exceptions from cancellation are swallowed.
                await asyncio.gather(*tasks, return_exceptions=True)
            if results:
                break

        # We override search(), so we must call the post-filter ourselves —
        # the base class only does it on its own code path. Mirror the base
        # class's diagnostics contract so the aggregator's per-engine drop
        # accounting still works.
        if diagnostics is not None:
            raw_count = len(results)
            filtered, drops = apply_post_filters_with_diagnostics(results, filters)
            diagnostics.setdefault("raw_per_engine", {})[self.name] = raw_count
            diagnostics.setdefault("after_filter_per_engine", {})[self.name] = len(filtered)
            agg = diagnostics.setdefault("drops_by_reason", {})
            for reason, n in drops.items():
                agg[reason] = agg.get(reason, 0) + n
            results = filtered[:max_results]
        else:
            results = apply_post_filters(results, filters)[:max_results]
        for i, r in enumerate(results):
            r.rank = i + 1
            r.engine = self.name
        return results
