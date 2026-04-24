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

from typing import Any

from .aggregator import aggregate_search
from .config import settings
from .fetcher import fetch_many
from .formatting import estimate_tokens


async def research(
    question: str,
    depth: int = 3,
    engines: list[str] | None = None,
    fetch: bool = True,
    use_cache: bool = True,
) -> dict[str, Any]:
    if not question.strip():
        raise ValueError("question must not be empty")
    depth = max(1, min(depth, 8))

    sr = await aggregate_search(
        question,
        engines=engines,
        max_results=max(depth * 2, depth + 3),
        use_cache=use_cache,
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
        results = await fetch_many(urls)
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
