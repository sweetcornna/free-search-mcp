"""Quark / 神马 Shenma (quark.sm.cn) — Alibaba's Chinese web index, HTML, no API key.

Shenma is Alibaba's own crawler index, served through the Quark browser's
search page. It is a separate Chinese opinion beside `baidu` and `so360`, and
strong on the Chinese developer web: measured 2026-10-31, "python asyncio
taskgroup" returned php.cn, CSDN, Toutiao and Aliyun articles, and
"python 异步编程" CSDN, Aliyun and Tencent Cloud, in ~2-3.5 s over plain HTTP.
`m.sm.cn` (Shenma's mobile front) renders the same cards but was slower in
every paired measurement (3.9 vs 3.1 s, 4.6 vs 3.2 s) and was walled first
when the two were queried alternately.

The page is a list of `div.sc` cards whose `data-sc` JSON names the card type.
Organic web results are `ss_text`, `ss_pic`, `ss_kv` and `nature_result`;
everything else is Quark's own furniture and is skipped: `text_recommend*`
(related searches), `baike` and `*entity*` knowledge panels, `news_uchq` and
`weibo_strong` carousels, `doc_sc*` (Quark Docs), `note_*` and `music_*`.
`ss_note` cards are rendered client-side and carry no links. Ads are not cards
at all but `div.cpc-card` units linking to `quark.sm.cn/adclick`. All organic
card types share Quark's `qk-*` UI components: the title is `.qk-title-text`,
the snippet `.qk-paragraph-text`, and the source line `.qk-source-item`
("CSDN博客 · 2026-10-15", or "3天前" = 3 days ago) holds the result's date. A
`nature_result` can also wrap Quark's own pages (a `page.sm.cn` video player
around a Bilibili clip); hosts under sm.cn / quark.cn are dropped.

Result anchors are direct target URLs, not click redirects, though Quark often
links the mobile host (m.blog.csdn.net, m.toutiao.com, m.php.cn). The card's
`norm_url` is not used instead: it sometimes names a different page
(m.cngold.org/insurance/lm1237/ vs insurance.cngold.org/gsdt/).

Measured live 2026-10-31: Quark honours NO query operators. Any query holding
`site:` or `-site:` returns its "没有找到相关的结果" (no results) page, and
`filetype:pdf` is read as words, so domain and PDF filters are left entirely
to the post-filter. `tl_request` is its own time filter (4 day, 3 week,
2 month, 1 year) and is honoured: with the week window every hit was 1-6 days
old. No safesearch parameter was found.

Quark answers a burst with Alibaba's x5sec slider page: a ~1.2 KB 200 whose
script redirects to `/_____tmd_____/punish?x5secdata=…`. Measured: the fourth
back-to-back request (about 6 s in) got it, it was gone 30 s later, and six
requests 6 s apart all passed. That page parses to `[]` like any other
unparseable page, per the keyless-engine contract.
"""

from __future__ import annotations

import json
from urllib.parse import quote_plus

from .base import (
    Engine,
    SearchFilters,
    SearchResult,
    enclosing_anchor,
    extract_date_hint,
    host_matches,
    host_of,
    inline_text,
    parse_html,
)

_ENDPOINT = "https://quark.sm.cn/s"

# Quark's `tl_request` values, from the page's own 发布时间 (publish time) menu.
_FRESHNESS = {"day": "4", "week": "3", "month": "2", "year": "1"}

_ORGANIC_CARDS = frozenset({"ss_text", "ss_pic", "ss_kv", "nature_result"})

# Quark's own pages: video wrappers on page.sm.cn, m.quark.cn verticals, and
# the quark.sm.cn/adclick redirect behind ads.
_OWN_HOSTS = ("sm.cn", "quark.cn")

def _is_own_host(url: str) -> bool:
    # A malformed href has no host: drop the row, keep the page.
    host = host_of(url)
    return not host or host_matches(host, _OWN_HOSTS)


class QuarkEngine(Engine):
    """Quark / Shenma web results (keyless HTML scrape)."""

    name = "quark"
    description = "Quark/Shenma (Alibaba): Chinese web index; a third opinion beside Baidu and 360."
    # Measured 2026-10-31: the fourth back-to-back request (about 6 s in) got
    # Alibaba's x5sec wall, which lifted 30 s later, while six requests 6 s
    # apart all passed. So ten a minute, at most three at once, and a search
    # skips Quark rather than queue more than a second for it.
    rate_limit_per_minute = 10
    rate_limit_burst = 3
    rate_limit_max_wait = 1.0
    # Alibaba's x5sec wall, its answer to a burst: a 1 KB page whose script
    # redirects to /_____tmd_____/punish?x5secdata=... and ends in an HTML
    # comment. Both are markup an echoed query cannot produce.
    gate_markers = (("captcha", ("_____tmd_____/punish?x5secdata=", "<!--rgv587_flag")),)

    def build_url(
        self, query: str, max_results: int, filters: SearchFilters | None = None
    ) -> str:
        # No operators: Quark answers any `site:`/`-site:` query with an empty
        # page and reads `filetype:` as words (see the module docstring), so
        # domain and PDF constraints are enforced by the post-filter alone.
        # One page of ~10 cards is all it serves; there is no count parameter.
        url = f"{_ENDPOINT}?q={quote_plus(query)}"
        if filters and filters.freshness:
            token = _FRESHNESS.get(filters.freshness)
            if token:
                url += f"&tl_request={token}"
        return url

    def parse(self, html: str) -> list[SearchResult]:
        if not html:
            return []
        tree = parse_html(html)
        results: list[SearchResult] = []
        seen: set[str] = set()

        for card in tree.css("div.sc[data-sc]"):
            try:
                meta = json.loads(card.attributes.get("data-sc") or "")
            except ValueError:
                continue
            if not isinstance(meta, dict) or meta.get("sc") not in _ORGANIC_CARDS:
                continue
            # `ads` is 0 on every organic card seen; honour it if that changes.
            if meta.get("ads"):
                continue
            head = card.css_first(".qk-title-text")
            anchor = enclosing_anchor(head)
            if anchor is None:
                continue
            url = (anchor.attributes.get("href") or "").strip()
            title = inline_text(head)
            if not title or not url.startswith(("http://", "https://")):
                continue
            if _is_own_host(url) or url in seen:
                continue
            seen.add(url)

            snippet = inline_text(card.css_first(".qk-paragraph-text"))
            source = " · ".join(inline_text(n) for n in card.css(".qk-source-item"))
            results.append(
                SearchResult(
                    title=title,
                    url=url,
                    snippet=snippet,
                    engine=self.name,
                    rank=0,
                    # Scraped from the page, so NOT confident; the absolute
                    # form is display-only under a freshness filter.
                    published_age=extract_date_hint(source) or extract_date_hint(snippet),
                )
            )
        return results
