"""LLM-friendly output formatters.

Three concerns:
1. Token estimation — char heuristic (4ch/token Latin, 2ch/token CJK), good
   enough for budgeting without paying for the tiktoken dependency.
2. Smart truncation — break at paragraph > newline > sentence, never mid-word.
3. Markdown views of every tool result so the LLM gets readable text rather
   than a JSON blob it has to parse before reading.
"""
from __future__ import annotations

from typing import Any


def estimate_tokens(text: str) -> int:
    """Rough token count without a tokenizer.

    Latin scripts ≈ 4 chars/token, CJK ≈ 1 char/token. Off by 10–20% on weird
    inputs but plenty good for a 'is this going to blow my context window?'
    decision.
    """
    if not text:
        return 0
    cjk = 0
    for c in text:
        o = ord(c)
        if 0x4E00 <= o <= 0x9FFF or 0x3040 <= o <= 0x30FF or 0xAC00 <= o <= 0xD7A3:
            cjk += 1
    latin = len(text) - cjk
    return cjk + max(1, latin // 4)


_BOUNDARIES = ("\n\n", "\n", "。", ". ", "！", "! ", "？", "? ")


def smart_truncate(text: str, max_chars: int) -> tuple[str, bool]:
    """Truncate at the latest natural boundary within the budget.

    Refuses to cut more than 30% past the boundary — falls back to a hard cut
    if the only boundary is way too early.
    """
    if len(text) <= max_chars:
        return text, False
    head = text[:max_chars]
    floor = int(max_chars * 0.7)
    best = -1
    for sep in _BOUNDARIES:
        idx = head.rfind(sep)
        if idx >= floor and idx + len(sep) > best:
            best = idx + len(sep)
    if best <= 0:
        return head.rstrip() + " …", True
    return head[:best].rstrip() + "\n\n[…truncated]", True


def render_search(payload: dict[str, Any]) -> str:
    """Render aggregator output as a numbered Markdown list with provenance."""
    query = payload.get("query", "")
    engines = ", ".join(payload.get("engines") or [])
    results = payload.get("results") or []
    errors = payload.get("errors") or {}
    cached = payload.get("cached")

    lines = [f"# Search: {query}", "", f"_engines: {engines}_  _results: {len(results)}_"]
    if cached:
        lines.append("_(from cache)_")
    lines.append("")

    if not results:
        lines.append("**No results.** Try a broader query, different engines, "
                     "or check `errors` if any engine failed.")
        if errors:
            lines.append("")
            for name, err in errors.items():
                lines.append(f"- {name}: {err}")
        return "\n".join(lines)

    for i, r in enumerate(results, 1):
        title = (r.get("title") or "(untitled)").strip()
        url = r.get("url") or ""
        snippet = (r.get("snippet") or "").strip()
        engines_for = ", ".join(r.get("engines") or [])
        score = r.get("score")
        meta = f"_{engines_for}_" + (f" · score {score}" if score is not None else "")
        lines.append(f"## {i}. {title}")
        lines.append(f"<{url}>")
        if snippet:
            lines.append("")
            lines.append(f"> {snippet}")
        lines.append("")
        lines.append(meta)
        lines.append("")

    if errors:
        lines.append("---")
        lines.append("**Engine errors (non-fatal):**")
        for name, err in errors.items():
            lines.append(f"- {name}: {err}")

    return "\n".join(lines).rstrip() + "\n"


def render_fetch(result: dict[str, Any]) -> str:
    """Render a fetched page as a Markdown document with metadata header."""
    url = result.get("url", "")
    title = result.get("title") or "(untitled)"
    method = result.get("method", "")
    truncated = result.get("truncated", False)
    tokens = result.get("tokens_estimated")
    author = result.get("author") or ""
    published_date = result.get("published_date") or ""
    sitename = result.get("sitename") or ""
    content = result.get("content") or ""

    byline_parts: list[str] = []
    if sitename:
        byline_parts.append(sitename)
    if author:
        byline_parts.append(f"by {author}")
    if published_date:
        byline_parts.append(published_date)
    byline = " · ".join(byline_parts)

    meta_line = (
        f"_fetched via {method}_"
        + (f" · ~{tokens} tokens" if tokens else "")
        + (" · truncated" if truncated else "")
    )

    header = [f"# {title}", f"<{url}>"]
    if byline:
        header.append(f"_{byline}_")
    header.append(meta_line)
    header.append("")
    return "\n".join(header) + content.rstrip() + "\n"


def render_doc(result: dict[str, Any]) -> str:
    source = result.get("source", "")
    fmt = result.get("format", "")
    title = result.get("title") or ""
    pages = result.get("pages")
    truncated = result.get("truncated", False)
    tokens = result.get("tokens_estimated")
    start = result.get("start", 0)
    length = result.get("returned_chars")
    content = result.get("content") or ""

    parts = [f"_{fmt}: {source}_"]
    if pages:
        parts.append(f"{pages} pages")
    if tokens:
        parts.append(f"~{tokens} tokens")
    if start or length:
        parts.append(f"slice [{start}:{(start + length) if length else ''}]")
    if truncated:
        parts.append("truncated")
    head = " · ".join(parts)

    title_line = f"# {title}\n\n" if title else ""
    return f"{title_line}{head}\n\n{content.rstrip()}\n"


def render_research(payload: dict[str, Any]) -> str:
    question = payload.get("question", "")
    sources = payload.get("sources") or []
    docs = payload.get("documents") or []
    tokens = payload.get("tokens_estimated")
    engines = ", ".join(payload.get("engines") or [])

    lines = [f"# Research brief: {question}", ""]
    meta = [f"engines: {engines}", f"sources: {len(sources)}"]
    if tokens:
        meta.append(f"~{tokens} tokens")
    lines.append("_" + " · ".join(meta) + "_")
    lines.append("")

    lines.append("## Sources")
    for s in sources:
        lines.append(f"- [{s.get('rank')}] **{s.get('title')}** — <{s.get('url')}>")
        sn = (s.get("snippet") or "").strip()
        if sn:
            lines.append(f"    > {sn}")
    lines.append("")

    if docs:
        lines.append("## Documents")
        lines.append("")
        for d in docs:
            if "error" in d:
                lines.append(f"### ⚠ {d.get('url')}")
                lines.append(f"_failed: {d.get('error')}_")
                lines.append("")
                continue
            title = d.get("title") or "(untitled)"
            url = d.get("url", "")
            tok = d.get("tokens_estimated")
            tcrumb = f" · ~{tok} tokens" if tok else ""
            lines.append(f"### {title}")
            lines.append(f"<{url}>{tcrumb}")
            lines.append("")
            lines.append((d.get("content") or "").rstrip())
            lines.append("")
            lines.append("---")
            lines.append("")
    return "\n".join(lines).rstrip() + "\n"


def errors_to_hint(errors: dict[str, str] | None) -> str | None:
    """Translate engine errors into an actionable hint for the LLM."""
    if not errors:
        return None
    failed = list(errors.keys())
    return (
        f"Engines that failed: {', '.join(failed)}. "
        "If results are thin, retry the call with `engines=` set to the working ones, "
        "or rephrase the query."
    )
