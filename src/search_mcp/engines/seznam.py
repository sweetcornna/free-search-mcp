"""Seznam (search.seznam.cz) — independent Czech web index, HTML page, no API key.

Seznam runs its own crawler and index rather than reselling Bing or Google, so
it is a genuinely separate opinion. Despite the Czech interface it ranks
English pages well: measured 2026-10-31, "python asyncio taskgroup exception
handling" returned Real Python, the CPython issue tracker and SuperFastPython,
and every query answered in 0.4-1.9 s over plain HTTP with no consent or
captcha wall. English ranking is not uniformly clean: "rust tokio select
macro" mixed in a camera lens and a Tokyo hotel.

The page is a server-rendered React app whose class names are per-build
hashes (`Uc5ecc67d33`, `f2c528`), so it is NOT scraped from the markup. The
same render embeds its whole state as JSON in
`<script id="renderer-state-data" data-state="...">`, and there every block
names its service: organic hits are `ftxt-organic/result`, ads are
`sklik/skliktop|sklikbottom`, and the widgets (`firmy` business map, `zbozi`
shopping, `related` queries, `wikicard2` knowledge panel, `video`, image
strips) each have their own. Reading that state is sturdier and also more
complete: on "volby 2026" the markup rendered 3 of the 10 organic hits (the
rest hydrate client-side) while the state held all 10. A hit's sitelinks
(`…/#python-311-alpha`, `#other-new-features`) live in its own
`relatedDocuments`, so each organic block yields exactly one result.

Measured live 2026-10-31: `site:`, `-site:` and `filetype:pdf` are honoured.
There is no time filter in the UI or the state, so freshness is left to the
post-filter. `count` accepts 10 or 20; larger values redirect back to 10.
Adult filtering is on by default (`pornFilterLevel: "noporn"` in the state)
and no URL parameter for it was found, so `safesearch` is not forwarded.

Best-effort by nature: this reads a page meant for humans. An unparseable
page, or one without the state blob, yields `[]` rather than an error, per the
keyless-engine contract.
"""

from __future__ import annotations

import json
from collections.abc import Iterator
from datetime import UTC, datetime
from typing import Any
from urllib.parse import quote_plus

from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    augment_query_with_operators,
    extract_date_hint,
    inline_text,
    parse_html,
)

_ENDPOINT = "https://search.seznam.cz/"


def _organic_entities(node: Any) -> Iterator[dict]:
    """Yield the `ftxt-organic/result` blocks of the state tree, in page order.

    The tree nests layout containers (`layout/legacy-left`, ...) around the
    blocks, and their depth differs between pages, so this walks it rather
    than indexing a fixed path. It does not descend into a hit it has yielded.
    """
    if isinstance(node, dict):
        ident = node.get("id")
        if (
            isinstance(ident, dict)
            and ident.get("service") == "ftxt-organic"
            and ident.get("type") == "result"
        ):
            yield node
            return
        for value in node.values():
            yield from _organic_entities(value)
    elif isinstance(node, list):
        for value in node:
            yield from _organic_entities(value)


def _plain(fragment: Any) -> str:
    """Text of an HTML fragment such as `<b class=sec>asyncio</b>.TaskGroup&hellip;`.

    Joined with no separator: Seznam bolds query terms inside words and
    dotted names, and a space at each tag boundary printed "asyncio . TaskGroup".
    """
    if not isinstance(fragment, str) or not fragment:
        return ""
    return inline_text(parse_html(fragment))


def _shown_date(data: dict) -> str:
    """The date Seznam itself prints for a hit, as YYYY-MM-DD, else "".

    Seznam prints `attributes.publicationDate` only on video and forum hits
    (those carrying `video` / `forumPosting`). On other pages the same field
    looks like a first-crawl time: docs.python.org's asyncio-task page is dated
    2014-04-25, a news section front 2018. Surfacing it there would tell a
    reader the current Python docs are twelve years stale.
    """
    if "video" not in data and "forumPosting" not in data:
        return ""
    stamp = (data.get("attributes") or {}).get("publicationDate")
    if not isinstance(stamp, (int, float)) or isinstance(stamp, bool) or stamp <= 0:
        return ""
    try:
        return datetime.fromtimestamp(stamp, tz=UTC).strftime("%Y-%m-%d")
    except (OverflowError, OSError, ValueError):
        return ""


class SeznamEngine(Engine):
    """Seznam web results (keyless, from the page's embedded state)."""

    name = "seznam"
    description = "Seznam.cz: independent Czech crawler index; strong on Czech, solid on English."

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        filetype = "pdf" if filters and filters.category == "pdf" else None
        q = augment_query_with_operators(
            query,
            include_domains=filters.include_domains if filters else None,
            exclude_domains=filters.exclude_domains if filters else None,
            filetype=filetype,
        )
        url = f"{_ENDPOINT}?q={quote_plus(q)}"
        # 10 is the default page; 20 is the only larger size Seznam accepts.
        if max_results > 10:
            url += "&count=20"
        return url

    def parse(self, html: str) -> list[SearchResult]:
        if not html:
            return []
        node = parse_html(html).css_first("script#renderer-state-data")
        raw = node.attributes.get("data-state") if node is not None else None
        if not raw:
            return []
        try:
            state = json.loads(raw)
        except ValueError:
            return []
        if not isinstance(state, dict):
            return []
        data = state.get("data")
        # Anything but the shape measured is an unparseable page, not an error.
        entities = data.get("entities") if isinstance(data, dict) else None

        results: list[SearchResult] = []
        seen: set[str] = set()
        for entity in _organic_entities(entities):
            data = entity.get("data")
            if not isinstance(data, dict):
                continue
            snip = data.get("snippet")
            if not isinstance(snip, dict):
                continue
            url = snip.get("url")
            if not isinstance(url, str):
                continue
            url = url.strip()
            title = _plain(snip.get("title"))
            if not title or not url.startswith(("http://", "https://")):
                continue
            if url in seen:
                continue
            seen.add(url)
            snippet = _plain(snip.get("description"))
            results.append(
                SearchResult(
                    title=title,
                    url=url,
                    snippet=snippet,
                    engine=self.name,
                    rank=0,
                    # Not confident: even the printed date is Seznam's guess,
                    # and freshness must never drop a hit on a guessed date.
                    published_age=_shown_date(data) or extract_date_hint(snippet),
                )
            )
        return results
