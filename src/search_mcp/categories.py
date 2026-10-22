"""The category and freshness vocabularies, in a module with no heavy imports.

The tool signatures in server.py are typed with these, so they are needed when
the server module loads. They used to live in engines/base.py, which pulls in
curl_cffi and selectolax, and through them lxml and bs4.
"""

from __future__ import annotations

from typing import Literal

Freshness = Literal["day", "week", "month", "year"]

# Two aliases, two jobs. Keeping them apart is what lets the dotted sub-category
# tokens exist without every `filters.category == "pdf"` site in the engines
# having to learn about them.
#
#   CategoryGroup — the INTERNAL filter predicate. `SearchFilters.category` is
#       always one of these, never dotted: `aggregate_search` splits the caller's
#       token at the first "." before building the filters. So the post-filter
#       branches below, `finalize_results`, and the eleven engines that special-
#       case `category == "pdf"` all keep comparing bare strings.
#
#   Category — the AGENT-FACING enum. `server.search` / `server.research` type
#       their `category` parameter with this, so the JSON Schema the model reads
#       lists every group AND every sub-group. That enum is the only taxonomy an
#       LLM reliably sees at call time, which is why the sub-groups live here
#       rather than in a lookup table it would have to go fetch.
#
# A bare group WIDENS (the specialists for that group, round-robined across
# sub-groups so one sub-group can't monopolise `category_engine_limit`); a
# dotted sub-group NARROWS to the sources that index exactly that.
CategoryGroup = Literal[
    "news", "pdf", "github", "paper", "forum", "blog", "image", "dataset", "finance",
    "software", "security", "reference", "weather", "docs", "gov", "stats", "calendar",
]
Category = Literal[
    # Groups
    "news", "pdf", "github", "paper", "forum", "blog", "image", "dataset", "finance",
    "software", "security", "reference", "weather", "docs", "gov", "stats", "calendar",
    # Sub-groups. Each must be `<group>.<name>` with `<group>` in CategoryGroup,
    # and at least one registered engine must declare it — both are asserted in
    # tests/test_source_taxonomy.py.
    "news.world",
    "paper.index",
    "paper.preprint",
    "paper.biomed",
    "paper.cs",
    "paper.math",
    "paper.openaccess",
    "paper.trial",
    "dataset.repository",
    "dataset.ml",
    "dataset.gov",
    "finance.filings",
    "finance.market",
    "finance.macro",
    "finance.fx",
    "finance.entity",
    "finance.crypto",
    "software.lifecycle",
    "software.github",
    "software.python",
    "software.node",
    "software.rust",
    "software.registry",
    "software.app",
    "security.cve",
    "security.package",
    "security.exploited",
    "reference.domain",
    "docs.web",
    "docs.rfc",
    "gov.us",
    "gov.uk",
    "stats.indicator",
    "calendar.holidays",
    "calendar.clock",
]


def category_group(category: str | None) -> str | None:
    """The group half of a category token: `"paper.biomed"` -> `"paper"`.

    Bare groups pass through unchanged, `None` stays `None`.
    """
    if not category:
        return None
    return category.split(".", 1)[0]
