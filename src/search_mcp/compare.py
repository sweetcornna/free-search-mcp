"""Side-by-side URL comparison.

Fetches multiple URLs concurrently via the existing fetcher and returns
per-URL excerpts that the LLM can compare against a single question. We do
not synthesise the answer here — that's the LLM's job; this module just
delivers structured material in one round trip.
"""
from __future__ import annotations

from typing import Any

from .fetcher import fetch_many
from .formatting import estimate_tokens, smart_truncate

# Cap each excerpt so 5 URLs comfortably fit a single LLM call's context window.
_PER_URL_CHAR_BUDGET = 6000

# Hard limits on URL count: <2 is a single fetch (use `fetch`); >5 is a batch
# (use `fetch_batch`). Compare exists for the small-N triangulation case.
_MIN_URLS = 2
_MAX_URLS = 5


async def compare_urls(question: str, urls: list[str]) -> dict[str, Any]:
    """Fetch each URL and return per-URL excerpts for `question`.

    Returns a dict with keys:
      - question: the original question
      - urls: the input URL list
      - excerpts: list of {url, title, sitename, published_date, excerpt,
                  truncated, tokens_estimated} or {url, error} on failure
      - tokens_estimated: total token estimate across all excerpts
    """
    if not _MIN_URLS <= len(urls) <= _MAX_URLS:
        raise ValueError(
            f"compare expects {_MIN_URLS}-{_MAX_URLS} URLs, got {len(urls)}"
        )

    results = await fetch_many(urls)
    excerpts: list[dict[str, Any]] = []
    for url, r in zip(urls, results):
        if isinstance(r, dict) and "error" in r:
            excerpts.append({"url": url, "error": r["error"]})
            continue
        d = r.to_dict() if hasattr(r, "to_dict") else r
        body = d.get("content") or ""
        body, truncated = smart_truncate(body, _PER_URL_CHAR_BUDGET)
        excerpts.append({
            "url": d.get("url") or url,
            "title": d.get("title", ""),
            "sitename": d.get("sitename", ""),
            "published_date": d.get("published_date", ""),
            "excerpt": body,
            "truncated": truncated,
            "tokens_estimated": estimate_tokens(body),
        })
    return {
        "question": question,
        "urls": urls,
        "excerpts": excerpts,
        "tokens_estimated": sum(e.get("tokens_estimated", 0) for e in excerpts),
    }


__all__ = ["compare_urls"]
