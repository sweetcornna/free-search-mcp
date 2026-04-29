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

    # Extractive lead from the top result that mentions the query terms — sits
    # above the result list so the model sees an answer-shaped fragment first.
    lead = payload.get("lead_snippet")
    if lead:
        lines.append(f"> **Lead:** {lead}")
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
        if r.get("published_age"):
            meta += f" · {r['published_age']}"
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

    diag = payload.get("filter_diagnostics")
    if diag:
        lines.extend(_render_filter_diagnostics(diag))

    return "\n".join(lines).rstrip() + "\n"


def _render_filter_diagnostics(diag: dict[str, Any]) -> list[str]:
    """Format the filter_diagnostics block as Markdown lines.

    Sits AFTER the result list (and after any engine-error block) because
    it's a meta-explanation, not a result. Marked clearly so the LLM can
    spot it and decide whether to retry with looser filters.
    """
    raw_per_engine = diag.get("raw_per_engine") or {}
    after_per_engine = diag.get("after_filter_per_engine") or {}
    drops = diag.get("drops_by_reason") or {}
    hint = diag.get("hint") or ""

    raw_total = sum(raw_per_engine.values())
    after_total = sum(after_per_engine.values())
    n_engines = len(raw_per_engine) or len(after_per_engine)

    lines: list[str] = []
    lines.append("")
    lines.append("---")
    lines.append("⚠️ **Filter diagnostics** (results were sparse)")
    lines.append("")
    lines.append(
        f"Raw results: {raw_total} across {n_engines} engine"
        f"{'s' if n_engines != 1 else ''} → {after_total} after filters."
    )
    if drops:
        # Sort by drop count desc so the worst offender leads.
        ordered = sorted(drops.items(), key=lambda kv: kv[1], reverse=True)
        top = ", ".join(f"{name} ({n})" for name, n in ordered)
        lines.append(f"Top drops: {top}.")
    if hint:
        lines.append("")
        lines.append(f"Hint: {hint}")
    return lines


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


def render_compare(payload: dict[str, Any]) -> str:
    """Render a `compare_urls` payload as a Markdown brief, one section per URL."""
    question = payload.get("question", "")
    excerpts = payload.get("excerpts") or []
    tokens = payload.get("tokens_estimated")
    lines = [f"# Compare: {question}", ""]
    if tokens:
        lines.append(f"_~{tokens} tokens across {len(excerpts)} URLs_")
        lines.append("")
    for i, e in enumerate(excerpts, 1):
        if "error" in e:
            lines.append(f"## {i}. ⚠ {e['url']}")
            lines.append(f"_failed: {e['error']}_")
            lines.append("")
            continue
        lines.append(f"## {i}. {e.get('title') or '(untitled)'}")
        lines.append(f"<{e['url']}>")
        meta_bits: list[str] = []
        if e.get("sitename"):
            meta_bits.append(e["sitename"])
        if e.get("published_date"):
            meta_bits.append(e["published_date"])
        if e.get("truncated"):
            meta_bits.append("truncated")
        if meta_bits:
            lines.append(f"_{' · '.join(meta_bits)}_")
        lines.append("")
        lines.append((e.get("excerpt") or "").rstrip())
        lines.append("")
        lines.append("---")
        lines.append("")
    return "\n".join(lines).rstrip() + "\n"


_STRUCTURED_KEYS = ("json_ld", "microdata", "opengraph", "rdfa", "microformat")


def render_structured(payload: dict[str, Any]) -> str:
    """Render an `extract_structured` payload as Markdown with JSON code blocks.

    Surfaces a top-of-page hint (when ``hint`` is set) and a Meta-tags table
    (when ``meta_fallback`` is set) so callers can tell apart "no data" from
    "blocked by bot shield".
    """
    import json

    url = payload.get("url", "")
    lines = [f"# Structured data: {url}", ""]

    hint = payload.get("hint")
    if hint:
        lines.append("> **No structured data found.**")
        lines.append(">")
        lines.append(f"> {hint}")
        lines.append("")

    any_section = False
    for key in _STRUCTURED_KEYS:
        items = payload.get(key) or []
        if not items:
            continue
        any_section = True
        lines.append(f"## {key}")
        for it in items:
            lines.append("```json")
            lines.append(json.dumps(it, ensure_ascii=False, indent=2)[:2000])
            lines.append("```")
        lines.append("")

    meta_fallback = payload.get("meta_fallback") or {}
    if meta_fallback:
        lines.append("## Meta tags")
        lines.append("")
        lines.append("| key | value |")
        lines.append("| --- | --- |")
        for k, v in meta_fallback.items():
            # Escape pipes in values so the Markdown table doesn't break.
            safe_v = str(v).replace("|", "\\|").replace("\n", " ").strip()
            if len(safe_v) > 200:
                safe_v = safe_v[:200] + " …"
            lines.append(f"| `{k}` | {safe_v} |")
        lines.append("")
        any_section = True

    if not any_section and not hint:
        # Defensive: payload had no syntaxes and no hint (shouldn't happen
        # with the new extractor, but keeps render side-effect free).
        lines.append("_No structured data found on this page._")
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
