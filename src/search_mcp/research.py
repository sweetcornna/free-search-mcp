"""Composed `research` workflow: search → fetch top N → return everything.

The motivation is round-trip reduction. A naive LLM workflow looks like:
    1. call search
    2. read results, decide which URLs look good
    3. call fetch on URL #1
    4. call fetch on URL #2
    5. call fetch on URL #3
That's 5 turns, 4 of which the LLM spends just reasoning about which URL to
read next. `research(question, depth=3)` collapses it into a single turn that
already includes the actual page text — same total tokens, far fewer turns.
"""
from __future__ import annotations

import asyncio
from typing import Any, Literal

from .aggregator import aggregate_search
from .cache import cache
from .fetcher import fetch_many, fetch_page
from .formatting import estimate_tokens


async def _fetch_with_freshness(
    urls: list[str], page_max_age_seconds: int | None,
) -> list[Any]:
    """Fetch page bodies, honoring a per-page freshness ceiling.

    When ``page_max_age_seconds`` is None we defer to the shared ``fetch_many``
    (which serves whatever is cached within the default TTL). When it is set we
    pre-check each page's age via ``cache.get_page(..., max_age_seconds=...)`` —
    a miss means the cached body is too old, so we re-fetch that URL with
    ``force_refresh=True``. ``page_max_age_seconds == 0`` forces every page to be
    re-fetched. Per-URL errors are captured as ``{"url", "error"}`` dicts, same
    contract as ``fetch_many``.
    """
    if page_max_age_seconds is None:
        return await fetch_many(urls)

    async def one(u: str):
        try:
            force = page_max_age_seconds == 0
            if not force:
                cached = await cache.get_page(u, max_age_seconds=page_max_age_seconds)
                force = cached is None
            return await fetch_page(u, force_refresh=force)
        except Exception as e:  # mirror fetch_many's per-URL error capture
            return {"url": u, "error": str(e)}

    return await asyncio.gather(*(one(u) for u in urls))


async def research(
    question: str,
    depth: int = 3,
    engines: list[str] | None = None,
    fetch: bool = True,
    use_cache: bool = True,
    *,
    max_age_seconds: int | None = None,
    page_max_age_seconds: int | None = None,
    freshness: Literal["day", "week", "month", "year"] | None = None,
    include_domains: list[str] | None = None,
    exclude_domains: list[str] | None = None,
    category: Literal["news", "pdf", "github", "paper", "forum", "blog"] | None = None,
    include_text: str | None = None,
    exclude_text: str | None = None,
) -> dict[str, Any]:
    if not question.strip():
        raise ValueError("question must not be empty")
    depth = max(1, min(depth, 8))

    sr = await aggregate_search(
        question,
        engines=engines,
        max_results=max(depth * 2, depth + 3),
        use_cache=use_cache,
        max_age_seconds=max_age_seconds,
        freshness=freshness,
        include_domains=include_domains,
        exclude_domains=exclude_domains,
        category=category,
        include_text=include_text,
        exclude_text=exclude_text,
    )

    top = sr["results"][:depth]
    sources = [
        {
            "rank": i,
            "title": r.get("title", ""),
            "url": r.get("url", ""),
            "snippet": r.get("snippet", ""),
            "engines": r.get("engines", []),
            "score": r.get("score"),
        }
        for i, r in enumerate(top, 1)
    ]

    docs: list[dict[str, Any]] = []
    if fetch and sources:
        urls = [s["url"] for s in sources]
        results = await _fetch_with_freshness(urls, page_max_age_seconds)
        for src, r in zip(sources, results):
            if isinstance(r, dict) and "error" in r:
                docs.append({"url": src["url"], "error": r["error"]})
            else:
                d = r.to_dict() if hasattr(r, "to_dict") else r
                d["title"] = d.get("title") or src["title"]
                docs.append(d)

    total_tokens = sum(d.get("tokens_estimated", 0) for d in docs)
    return {
        "question": question,
        "engines": sr.get("engines"),
        "sources": sources,
        "documents": docs if fetch else [],
        "tokens_estimated": total_tokens or estimate_tokens(
            "\n".join(s.get("snippet", "") for s in sources)
        ),
        "errors": sr.get("errors"),
    }
