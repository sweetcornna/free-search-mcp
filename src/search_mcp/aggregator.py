from __future__ import annotations

import asyncio
import hashlib
import json
import logging
from dataclasses import asdict
from typing import Any, Literal

from .cache import cache
from .config import settings
from .engines import ENGINES, SearchFilters, SearchResult, get_engine
from .ratelimit import RateLimiter

log = logging.getLogger(__name__)
search_limiter = RateLimiter(settings.rate_limit_per_minute)


def _normalize_url(url: str) -> str:
    if url.startswith("//"):
        url = "https:" + url
    return url.split("#", 1)[0].rstrip("/")


def _key(query: str, engines: list[str], max_results: int, filters: SearchFilters) -> str:
    raw = json.dumps(
        {
            "q": query,
            "e": sorted(engines),
            "n": max_results,
            "f": asdict(filters),
        },
        ensure_ascii=False,
        sort_keys=True,
    )
    return hashlib.sha256(raw.encode("utf-8")).hexdigest()


def _merge(buckets: list[list[SearchResult]], max_results: int) -> list[dict[str, Any]]:
    """Reciprocal-rank-fusion across engines: same URL appearing high in multiple
    engines wins. Stable, no scoring magic, well-known IR technique."""
    k = 60.0
    scores: dict[str, float] = {}
    representative: dict[str, dict[str, Any]] = {}
    engines_for: dict[str, list[str]] = {}

    for bucket in buckets:
        for r in bucket:
            url = _normalize_url(r.url)
            if not url:
                continue
            scores[url] = scores.get(url, 0.0) + 1.0 / (k + r.rank)
            engines_for.setdefault(url, []).append(r.engine)
            if url not in representative or len(r.snippet) > len(representative[url].get("snippet", "")):
                representative[url] = r.to_dict()
                representative[url]["url"] = url

    ranked = sorted(scores.items(), key=lambda kv: kv[1], reverse=True)
    out = []
    for url, score in ranked[:max_results]:
        rec = representative[url]
        rec["engines"] = sorted(set(engines_for[url]))
        rec["score"] = round(score, 5)
        rec.pop("rank", None)
        rec.pop("engine", None)
        out.append(rec)
    return out


async def aggregate_search(
    query: str,
    engines: list[str] | None = None,
    max_results: int | None = None,
    use_cache: bool = True,
    *,
    freshness: Literal["day", "week", "month", "year"] | None = None,
    include_domains: list[str] | None = None,
    exclude_domains: list[str] | None = None,
    category: Literal["news", "pdf", "github", "paper", "forum", "blog"] | None = None,
    include_text: str | None = None,
    exclude_text: str | None = None,
) -> dict[str, Any]:
    engine_names = engines or settings.default_engines
    n = max_results or settings.max_results_per_engine
    filters = SearchFilters(
        freshness=freshness,
        include_domains=list(include_domains) if include_domains else [],
        exclude_domains=list(exclude_domains) if exclude_domains else [],
        category=category,
        include_text=include_text,
        exclude_text=exclude_text,
    )
    cache_key = _key(query, engine_names, n, filters)

    if use_cache:
        hit = await cache.get_search(cache_key)
        if hit:
            return {"query": query, "engines": engine_names, "cached": True, "results": hit}

    async def run(name: str) -> tuple[str, list[SearchResult] | Exception]:
        try:
            engine = get_engine(name)
        except ValueError as e:
            return name, e
        await search_limiter.acquire(name)
        try:
            return name, await engine.search(query, n, filters)
        except Exception as e:
            log.warning("engine %s failed: %s", name, e)
            return name, e

    results = await asyncio.gather(*(run(n) for n in engine_names))
    buckets: list[list[SearchResult]] = []
    errors: dict[str, str] = {}
    for name, res in results:
        if isinstance(res, Exception):
            errors[name] = str(res)
        else:
            buckets.append(res)

    merged = _merge(buckets, n)

    if use_cache and merged:
        await cache.put_search(cache_key, query, engine_names, merged)

    return {
        "query": query,
        "engines": engine_names,
        "cached": False,
        "results": merged,
        "errors": errors or None,
    }


def list_engines() -> list[str]:
    return list(ENGINES.keys())
