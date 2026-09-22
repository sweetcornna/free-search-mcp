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
import re
from datetime import date
from typing import Any, Literal

from .aggregator import aggregate_search
from .cache import cache
from .engines import Category
from .fetcher import fetch_many, fetch_page
from .formatting import estimate_tokens


async def _read_within(urls: list[str], budget: float) -> list[Any]:
    """Fetch every URL, and stop waiting for the slow ones after `budget` seconds.

    `fetch_many` returns when the slowest page does, and one page behind a wall
    can take the whole `fetch_timeout` to fail (25.7 s measured for a Baidu
    Baike page, next to 0.5 s and 2.1 s for the two that mattered). A caller
    that wants an answer soon would rather have two pages now than three later.
    A page that misses the budget comes back as the same `{"url", "error"}`
    shape a failed fetch has, so its snippet is still used.
    """

    async def one(u: str) -> Any:
        try:
            return await fetch_page(u)
        except Exception as e:  # mirror fetch_many's per-URL error capture
            return {"url": u, "error": str(e)}

    tasks = [asyncio.ensure_future(one(u)) for u in urls]
    _, pending = await asyncio.wait(tasks, timeout=budget)
    for task in pending:
        task.cancel()
    if pending:
        await asyncio.gather(*pending, return_exceptions=True)
    return [
        {"url": u, "error": f"not read within {budget:g} s"} if t.cancelled() else t.result()
        for u, t in zip(urls, tasks, strict=True)
    ]


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


_ISO_DATE_RE = re.compile(r"\b(\d{4})-(\d{2})-(\d{2})\b")


def _date_note(sources: list[dict[str, Any]], docs: list[dict[str, Any]]) -> str:
    """A short note on how much of the brief can be dated, and how far apart.

    A page's own `published_date` (read from the document) outranks the
    search's `published_age` (read from a snippet). Only ISO dates are compared
    for the spread; "3 days ago" says enough on its own.
    """
    if not sources:
        return ""
    by_url = {d.get("url"): d for d in docs if isinstance(d, dict)}
    stamps: list[date] = []
    undated = 0
    for src in sources:
        doc = by_url.get(src.get("url")) or {}
        value = doc.get("published_date") or src.get("published_age") or ""
        if not value:
            undated += 1
            continue
        match = _ISO_DATE_RE.search(value)
        if match:
            try:
                stamps.append(date(int(match[1]), int(match[2]), int(match[3])))
            except ValueError:
                pass
    parts: list[str] = []
    if undated:
        parts.append(
            f"{undated} of {len(sources)} sources carry no publish date, so their age is "
            "unknown. Do not assume they are recent."
        )
    if len(stamps) >= 2 and (max(stamps) - min(stamps)).days > 365:
        parts.append(
            f"Dated sources span {min(stamps).isoformat()} to {max(stamps).isoformat()}. "
            "Where they disagree, prefer the newer one and say which you used."
        )
    return " ".join(parts)


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
    category: Category | None = None,
    include_text: str | None = None,
    exclude_text: str | None = None,
    read_budget_seconds: float | None = None,
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
            # What the search knew about each source's age and kind. These were
            # dropped here, so a research brief could not say whether the page
            # it was summarising was from this month or from 2019.
            **{k: r[k] for k in ("published_age", "date_source", "source_type") if r.get(k)},
        }
        for i, r in enumerate(top, 1)
    ]

    docs: list[dict[str, Any]] = []
    if fetch and sources:
        urls = [s["url"] for s in sources]
        if read_budget_seconds:
            results = await _read_within(urls, read_budget_seconds)
        else:
            results = await _fetch_with_freshness(urls, page_max_age_seconds)
        for src, r in zip(sources, results, strict=True):
            if isinstance(r, dict) and "error" in r:
                docs.append({"url": src["url"], "error": r["error"]})
            else:
                d = r.to_dict() if hasattr(r, "to_dict") else r
                d["title"] = d.get("title") or src["title"]
                docs.append(d)

    total_tokens = sum(d.get("tokens_estimated", 0) for d in docs)
    out = {
        "question": question,
        "engines": sr.get("engines"),
        "sources": sources,
        "documents": docs if fetch else [],
        "tokens_estimated": total_tokens or estimate_tokens(
            "\n".join(s.get("snippet", "") for s in sources)
        ),
        "errors": sr.get("errors"),
    }
    out["retrieved_at"] = sr.get("retrieved_at")
    if sr.get("cache_age_seconds") is not None:
        out["search_cache_age_seconds"] = sr["cache_age_seconds"]
    note = _date_note(sources, docs if fetch else [])
    if note:
        out["date_note"] = note
    # Carry the search's own explanation of a thin result set through to the
    # caller. Without this, `research(..., category="news")` that filtered every
    # hit away returned an empty brief with no stated reason — strictly less
    # informative than calling `search` with the same arguments.
    for key in (
        "filter_diagnostics",
        "gated_engines",
        "gated_hint",
        "empty_engines",
        "empty_hint",
        "refused_engines",
        "rate_limited_engines",
        "rate_limited_hint",
        "benched_engines",
        "benched_hint",
        "rescued_via",
        "freshness_note",
        "auto_routed",
        "timed_out_engines",
        "timed_out_hint",
    ):
        if sr.get(key):
            out[key] = sr[key]
    return out
