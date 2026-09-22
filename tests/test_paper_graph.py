"""`paper_graph` — citation-graph traversal (offline).

Every HTTP call goes through `paper_graph._api._get_json`, so a single stub
routed by URL substring covers the whole module. The stub records the URLs it
was asked for, which is how the request-count claims in the module docstring
("at most four requests regardless of `limit`") are actually held to.

Payload shapes are trimmed copies of real OpenAlex / Crossref responses.
"""

from __future__ import annotations

import os

import pytest

from search_mcp import paper_graph as pg
from search_mcp.formatting import render_paper_graph

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)

_TARGET = {
    "id": "https://openalex.org/W2148972377",
    "doi": "https://doi.org/10.1145/1571941.1572114",
    "display_name": "Reciprocal rank fusion outperforms condorcet",
    "publication_year": 2009,
    "cited_by_count": 671,
    "is_retracted": False,
    "primary_location": {"landing_page_url": "https://dl.acm.org/doi/10.1145/1571941.1572114"},
    "referenced_works": [
        "https://openalex.org/W2117665592",
        "https://openalex.org/W2153579005",
        "https://openalex.org/W2166560380",
    ],
    "authorships": [{"author": {"display_name": "Gordon V. Cormack"}}],
}

_REFS = {
    "results": [
        {
            "id": "https://openalex.org/W2117665592",
            "doi": "https://doi.org/10.1/low",
            "display_name": "Least cited reference",
            "publication_year": 2001,
            "cited_by_count": 12,
        },
        {
            "id": "https://openalex.org/W2153579005",
            "display_name": "Most cited reference",
            "publication_year": 2002,
            "cited_by_count": 3940,
            "primary_location": {"landing_page_url": "https://example.org/most"},
        },
        # A record OpenAlex could not restore is simply absent from `results`
        # — W2166560380 was asked for and did not come back.
    ]
}

_CITES = {
    "results": [
        {
            "id": "https://openalex.org/W3005",
            "display_name": "Image Quality Assessment",
            "publication_year": 2020,
            "cited_by_count": 931,
            "primary_location": {"landing_page_url": "https://example.org/iqa"},
        },
        {
            "id": "https://openalex.org/W3006",
            "display_name": "A retracted follow-up",
            "publication_year": 2021,
            "cited_by_count": 4,
            "is_retracted": True,
        },
    ]
}

_CROSSREF_OK = {"message": {"title": ["Reciprocal rank fusion"], "updated-by": []}}

_CROSSREF_RETRACTED = {
    "message": {
        "title": ["Hydroxychloroquine or chloroquine"],
        "updated-by": [
            {
                "type": "expression_of_concern",
                "DOI": "10.1016/s0140-6736(20)31290-3",
                "label": "Expression of concern",
                "updated": {"date-parts": [[2020, 6, 3]]},
            },
            {
                "type": "retraction",
                "DOI": "10.1016/s0140-6736(20)31324-6",
                "label": "Retraction",
                "updated": {"date-parts": [[2020, 6, 5]]},
            },
            # The SAME notice DOI, filed a second time under a milder type.
            {
                "type": "erratum",
                "DOI": "10.1016/s0140-6736(20)31324-6",
                "updated": {"date-parts": [[2020, 6, 13]]},
            },
        ],
    }
}


class _Api:
    """Routes by URL substring and records every request."""

    def __init__(self, **routes):
        self.routes = routes
        self.urls: list[str] = []

    async def __call__(self, url, **kw):
        self.urls.append(url)
        if "api.crossref.org" in url:
            return self.routes.get("crossref", _CROSSREF_OK)
        if "filter=cites:" in url:
            return self.routes.get("cites", _CITES)
        if "filter=openalex_id:" in url:
            return self.routes.get("refs", _REFS)
        if "?search=" in url:
            found = self.routes.get("search", _TARGET)
            return {"results": [found]} if found else {"results": []}
        return self.routes.get("target", _TARGET)


@pytest.fixture
def api(monkeypatch):
    stub = _Api()
    monkeypatch.setattr(pg._api, "_get_json", stub)
    return stub


# ---------------------------------------------------------------------------
# Identifier parsing
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    ("text", "expected"),
    [
        ("10.1145/1571941.1572114", ("doi", "10.1145/1571941.1572114")),
        ("https://doi.org/10.1145/1571941.1572114", ("doi", "10.1145/1571941.1572114")),
        ("doi:10.1145/1571941.1572114", ("doi", "10.1145/1571941.1572114")),
        # Pasted out of prose, trailing punctuation and all.
        ("(10.1145/1571941.1572114).", ("doi", "10.1145/1571941.1572114")),
        ("W2148972377", ("openalex", "W2148972377")),
        ("https://openalex.org/W2148972377", ("openalex", "W2148972377")),
        ("attention is all you need", ("title", "attention is all you need")),
        ("", ("title", "")),
    ],
)
def test_normalize_classifies_the_identifier(text, expected):
    assert pg._normalize(text) == expected


@pytest.mark.parametrize(
    ("parts", "expected"),
    [
        ({"date-parts": [[2020, 6, 5]]}, "2020-06-05"),
        ({"date-parts": [[2020, 6]]}, "2020-06"),
        ({"date-parts": [[2020]]}, "2020"),
        # Truncate at the gap rather than promoting the day into the month slot
        # — the same Crossref shape the crossref engine had to be fixed for.
        ({"date-parts": [[2020, None, 5]]}, "2020"),
        ({}, ""),
        (None, ""),
    ],
)
def test_crossref_date_truncates_at_the_first_gap(parts, expected):
    assert pg._crossref_date(parts) == expected


# ---------------------------------------------------------------------------
# Traversal
# ---------------------------------------------------------------------------


async def test_resolves_a_doi_against_the_works_endpoint(api):
    out = await pg.paper_graph("10.1145/1571941.1572114")
    assert out["resolved_as"] == "doi"
    assert out["paper"]["openalex_id"] == "W2148972377"
    assert api.urls[0].startswith(
        "https://api.openalex.org/works/https://doi.org/10.1145/1571941.1572114"
    )


async def test_resolves_a_title_through_search(api):
    out = await pg.paper_graph("Reciprocal rank fusion outperforms condorcet")
    assert out["resolved_as"] == "title"
    assert "?search=Reciprocal+rank+fusion" in api.urls[0]
    assert out["paper"]["title"].startswith("Reciprocal rank fusion")


# --- title resolution is a gate, not a guess -------------------------------


@pytest.mark.parametrize(
    ("query", "title", "confident"),
    [
        ("attention is all you need", "Attention Is All You Need", True),
        # A caller who typed only the opening words still means this paper.
        (
            "reciprocal rank fusion outperforms condorcet",
            "Reciprocal rank fusion outperforms condorcet and individual rank "
            "learning methods",
            True,
        ),
        # Words in common, different paper: the extra leading token changes the
        # subject. An intersection-based score calls this a perfect match.
        (
            "attention is all you need",
            "Channel Attention Is All You Need for Video Frame Interpolation",
            False,
        ),
        (
            "BERT pre-training of deep bidirectional transformers",
            "BEiT: BERT Pre-Training of Image Transformers",
            False,
        ),
        (
            "BERT pre-training of deep bidirectional transformers",
            "BioBERT: a pre-trained biomedical language representation model",
            False,
        ),
    ],
)
def test_title_score_separates_the_paper_from_its_neighbours(query, title, confident):
    assert (pg._title_score(query, title) >= pg._TITLE_MIN) is confident


def test_title_match_follows_relevance_order_not_the_highest_score():
    """All three of these answer to the typed title, and the string metric
    cannot tell which was meant — OpenAlex's relevance order can."""
    items = [
        {"display_name": "Attention Is All You Need"},
        {"display_name": "Attention Is All You Need In Speech Separation"},
    ]
    picked = pg._best_title_match("attention is all you need", items)
    assert picked["display_name"] == "Attention Is All You Need"


def test_no_candidate_clearing_the_gate_resolves_to_nothing():
    """OpenAlex answers "BERT pre-training of deep bidirectional transformers"
    with BioBERT, Sentence-BERT, T5, BEiT and AlphaFold — the real paper is not
    in the page. Taking the top hit returned a DIFFERENT paper's citation graph
    under the heading the caller typed, which a caller checking a citation has
    no way to catch."""
    items = [
        {"display_name": "BioBERT: a pre-trained biomedical language model"},
        {"display_name": "Sentence-BERT: Sentence Embeddings using Siamese Networks"},
        {"display_name": "BEiT: BERT Pre-Training of Image Transformers"},
    ]
    assert pg._best_title_match("BERT pre-training of deep bidirectional", items) is None


async def test_a_rejected_title_lists_the_near_misses(monkeypatch):
    """A dead end that names what the index DOES have is a next step; a bare
    "not found" reads as "this paper is not indexed"."""
    wrong = {
        "id": "https://openalex.org/W1",
        "display_name": "BEiT: BERT Pre-Training of Image Transformers",
    }
    monkeypatch.setattr(pg._api, "_get_json", _Api(search=wrong))
    out = await pg.paper_graph("BERT pre-training of deep bidirectional transformers")
    assert out["paper"] is None
    assert any("Closest titles found" in n for n in out["notes"])
    assert any("BEiT" in n for n in out["notes"])


async def test_a_doi_lookup_is_never_gated_on_similarity(api):
    """A DOI is an exact identifier; there is nothing to second-guess."""
    out = await pg.paper_graph("10.1145/1571941.1572114")
    assert out["paper"]["openalex_id"] == "W2148972377"


async def test_references_are_restored_in_one_batched_call(api):
    """One call, not one per reference — the whole reason `referenced_works`
    is an ID list and OpenAlex accepts an OR filter."""
    await pg.paper_graph("W2148972377", direction="references", limit=10)
    batched = [u for u in api.urls if "filter=openalex_id:" in u]
    assert len(batched) == 1
    assert "W2117665592|W2153579005|W2166560380" in batched[0]


async def test_references_are_ordered_by_influence_not_by_id(api):
    """OpenAlex returns `referenced_works` in an order that means nothing, so
    the most-cited reference has to be surfaced deliberately."""
    out = await pg.paper_graph("W2148972377", direction="references")
    assert [r["title"] for r in out["references"]] == [
        "Most cited reference",
        "Least cited reference",
    ]


async def test_unrestorable_references_are_reported_not_silently_dropped(api):
    """Three edges were recorded, two came back. Saying "2 references" would
    be a quiet lie about the paper's bibliography."""
    out = await pg.paper_graph("W2148972377", direction="references")
    assert any("of 3 references" in n for n in out["notes"])


async def test_citations_use_the_cites_filter_sorted_by_citation_count(api):
    """Sorting by date would answer "what is newest"; the question is "what did
    the field converge on"."""
    await pg.paper_graph("W2148972377", direction="citations", limit=5)
    cited = [u for u in api.urls if "filter=cites:" in u]
    assert len(cited) == 1
    assert "filter=cites:W2148972377" in cited[0]
    assert "sort=cited_by_count:desc" in cited[0]
    assert "per-page=5" in cited[0]


async def test_direction_skips_the_half_that_was_not_asked_for(api):
    await pg.paper_graph("W2148972377", direction="references")
    assert not [u for u in api.urls if "filter=cites:" in u]
    api.urls.clear()
    await pg.paper_graph("W2148972377", direction="citations")
    assert not [u for u in api.urls if "filter=openalex_id:" in u]


async def test_a_full_walk_costs_four_requests_regardless_of_limit(api):
    """Resolve, batched references, one citations page, one Crossref lookup."""
    await pg.paper_graph("W2148972377", limit=50)
    assert len(api.urls) == 4


async def test_limit_is_clamped_to_a_sane_range(api):
    out = await pg.paper_graph("W2148972377", limit=99999)
    assert len(out["citations"]) <= pg._MAX_LIMIT
    api.urls.clear()
    await pg.paper_graph("W2148972377", limit=0, direction="citations")
    assert "per-page=1" in [u for u in api.urls if "cites:" in u][0]


async def test_unknown_direction_falls_back_to_both(api):
    out = await pg.paper_graph("W2148972377", direction="sideways")
    assert out["direction"] == "both"


async def test_neighbour_retraction_flags_survive_into_the_payload(api):
    out = await pg.paper_graph("W2148972377", direction="citations")
    assert [c["retracted"] for c in out["citations"]] == [False, True]


# ---------------------------------------------------------------------------
# Verification
# ---------------------------------------------------------------------------


async def test_crossref_retraction_notice_marks_the_paper(monkeypatch):
    monkeypatch.setattr(
        pg._api, "_get_json", _Api(crossref=_CROSSREF_RETRACTED)
    )
    out = await pg.paper_graph("10.1016/S0140-6736(20)31180-6")
    assert out["paper"]["retracted"] is True
    assert out["notes"][0].startswith("RETRACTED")


async def test_notices_are_deduplicated_by_doi_keeping_the_worst_reading(
    monkeypatch,
):
    """Crossref files the SAME notice DOI under several types — the Lancet
    Surgisphere paper lists one as both `retraction` and `erratum`. Printing
    both buries the retraction in a list of errata."""
    monkeypatch.setattr(pg._api, "_get_json", _Api(crossref=_CROSSREF_RETRACTED))
    graph = await pg.paper_graph("10.1016/S0140-6736(20)31180-6")
    notices = graph["paper"]["crossref"]["notices"]
    assert [n["type"] for n in notices] == ["retraction", "expression_of_concern"]
    assert notices[0]["doi"] == "10.1016/s0140-6736(20)31324-6"
    assert notices[0]["date"] == "2020-06-05"


@pytest.mark.parametrize(
    ("kind", "worse_than"),
    [("retraction", "expression_of_concern"), ("expression_of_concern", "erratum")],
)
def test_notice_severity_ordering(kind, worse_than):
    assert pg._notice_rank(kind) < pg._notice_rank(worse_than)


def test_an_unknown_notice_type_sorts_last_but_is_still_shown():
    """Crossref's `type` vocabulary is open-ended; dropping what we don't
    recognise would hide a notice."""
    assert pg._notice_rank("something_new") > pg._notice_rank("addendum")


async def test_openalex_retraction_flag_alone_is_enough(monkeypatch):
    retracted = {**_TARGET, "is_retracted": True}
    monkeypatch.setattr(pg._api, "_get_json", _Api(target=retracted))
    out = await pg.paper_graph("W2148972377")
    assert out["paper"]["retracted"] is True


async def test_a_crossref_404_is_reported_as_a_datacite_doi_not_a_fake(monkeypatch):
    """Crossref registers journal DOIs; DataCite registers most dataset and
    repository DOIs. The work already resolved in OpenAlex, so a Crossref miss
    means "notices unavailable", not "fabricated"."""
    from search_mcp.engines import jsonapi

    class _NotFound(_Api):
        async def __call__(self, url, **kw):
            if "api.crossref.org" in url:
                self.urls.append(url)
                jsonapi._last_http_status.set(404)
                return None
            return await super().__call__(url, **kw)

    monkeypatch.setattr(pg._api, "_get_json", _NotFound())
    out = await pg.paper_graph("10.1145/1571941.1572114")
    assert out["paper"]["crossref"]["registered"] is False
    assert "DataCite" in out["notes"][0]


async def test_a_crossref_timeout_claims_nothing(monkeypatch):
    """`_get_json` maps a 404 and a timeout to the same None. Reporting the
    second as the first would be a confidently wrong answer about a real DOI."""
    monkeypatch.setattr(pg._api, "_get_json", _Api(crossref=None))
    out = await pg.paper_graph("10.1145/1571941.1572114")
    assert out["paper"]["crossref"]["registered"] is None
    assert not any("Crossref" in n for n in out["notes"])


async def test_a_paper_that_resolves_to_nothing_says_so(monkeypatch):
    monkeypatch.setattr(pg._api, "_get_json", _Api(target=None, search=None))
    out = await pg.paper_graph("10.9999/definitely-not-real")
    assert out["paper"] is None
    assert out["references"] == [] and out["citations"] == []
    assert "unregistered or mistyped" in out["notes"][0]


async def test_transport_failure_never_raises(monkeypatch):
    async def boom(url, **kw):
        return None

    monkeypatch.setattr(pg._api, "_get_json", boom)
    out = await pg.paper_graph("anything")
    assert out["paper"] is None


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------


async def test_render_puts_the_retraction_banner_before_anything_quotable(
    monkeypatch,
):
    monkeypatch.setattr(pg._api, "_get_json", _Api(crossref=_CROSSREF_RETRACTED))
    text = render_paper_graph(await pg.paper_graph("10.1016/S0140-6736(20)31180-6"))
    banner = text.index("RETRACTED PAPER")
    assert banner < text.index("# Reciprocal")
    assert "**Retraction** (2020-06-05)" in text


async def test_render_separates_the_two_directions(api):
    text = render_paper_graph(await pg.paper_graph("W2148972377"))
    assert "## References (what it builds on)" in text
    assert "## Cited by (what built on it)" in text
    assert text.index("## References") < text.index("## Cited by")
    assert "⚠️ **RETRACTED**" in text  # the retracted citing work


async def test_render_of_an_unresolved_paper_is_not_an_empty_page(monkeypatch):
    monkeypatch.setattr(pg._api, "_get_json", _Api(target=None, search=None))
    text = render_paper_graph(await pg.paper_graph("nonsense query"))
    assert "No paper matched" in text
    assert "nonsense query" in text


# ---------------------------------------------------------------------------
# Live network — opt-in
# ---------------------------------------------------------------------------


@skip_offline
async def test_live_walks_a_real_paper():
    out = await pg.paper_graph("10.1145/1571941.1572114", limit=3)
    assert out["paper"]["openalex_id"] == "W2148972377"
    assert out["references"] and out["citations"]
    assert out["paper"]["crossref"]["registered"] is True


@skip_offline
async def test_live_detects_a_known_retraction():
    out = await pg.paper_graph("10.1016/S0140-6736(20)31180-6", limit=2)
    assert out["paper"]["retracted"] is True


# ---------------------------------------------------------------------------
# arXiv identifiers
# ---------------------------------------------------------------------------
#
# OpenAlex answers 404 to arXiv's own DataCite DOIs, so the most natural thing
# to paste — the DOI printed on the arXiv page — used to come back as "the DOI
# may be unregistered or mistyped" for papers OpenAlex plainly indexes.

_ARXIV_FEED = """<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>arXiv Query: id_list=1706.03762</title>
  <entry>
    <id>http://arxiv.org/abs/1706.03762v7</id>
    <title>{title}</title>
  </entry>
</feed>"""


@pytest.mark.parametrize(
    ("given", "expected"),
    [
        ("10.48550/arXiv.1706.03762", ("arxiv", "1706.03762")),
        ("https://doi.org/10.48550/ARXIV.1706.03762", ("arxiv", "1706.03762")),
        ("https://arxiv.org/abs/1706.03762v7", ("arxiv", "1706.03762")),
        ("https://arxiv.org/pdf/1706.03762", ("arxiv", "1706.03762")),
        ("arXiv:1706.03762", ("arxiv", "1706.03762")),
        ("arXiv:hep-th/9901001v2", ("arxiv", "hep-th/9901001")),
        # The form people actually paste. Only when it is the whole input.
        ("1706.03762", ("arxiv", "1706.03762")),
        ("2401.12345v3", ("arxiv", "2401.12345")),
        ("hep-th/9901001", ("arxiv", "hep-th/9901001")),
        ("GPT-4 scored 1706.0376 on it", ("title", "GPT-4 scored 1706.0376 on it")),
        ("Results for 1706.03762 tokens", ("title", "Results for 1706.03762 tokens")),
        # Everything else is classified exactly as before.
        ("10.1145/1571941.1572114", ("doi", "10.1145/1571941.1572114")),
        ("W2741809807", ("openalex", "W2741809807")),
        ("Attention Is All You Need", ("title", "Attention Is All You Need")),
    ],
)
def test_arxiv_identifiers_are_recognised(given, expected):
    assert pg._normalize(given) == expected


def _arxiv(monkeypatch, *, title: str | None, **routes):
    stub = _Api(**routes)
    requested: list[str] = []

    async def text(url, **kw):
        requested.append(url)
        return _ARXIV_FEED.format(title=title) if title is not None else "<feed></feed>"

    monkeypatch.setattr(pg._api, "_get_json", stub)
    monkeypatch.setattr(pg._api, "_request_text", text)
    return stub, requested


async def test_an_arxiv_doi_resolves_through_the_title(monkeypatch):
    stub, requested = _arxiv(monkeypatch, title=_TARGET["display_name"])

    out = await pg.paper_graph("10.48550/arXiv.1706.03762")

    assert out["paper"]["title"] == _TARGET["display_name"]
    assert out["resolved_as"] == "arxiv"
    assert "id_list=1706.03762" in requested[0]
    # Never the 404: the arXiv DOI is not sent to OpenAlex's DOI endpoint.
    assert not any("10.48550" in url for url in stub.urls)
    assert any("?search=" in url for url in stub.urls)
    # A title match is a weaker identification than a DOI, and it says so.
    assert out["notes"][0].startswith("Resolved arXiv:1706.03762 by its title")
    assert out["references"] and out["citations"]


async def test_an_unknown_arxiv_id_says_so(monkeypatch):
    _arxiv(monkeypatch, title=None)
    out = await pg.paper_graph("arXiv:9999.99999")
    assert out["paper"] is None
    assert "arXiv returned no record for '9999.99999'" in out["notes"][0]
    assert "mistyped" not in out["notes"][0]


async def test_a_preprint_openalex_has_not_indexed_yet_says_that(monkeypatch):
    _arxiv(monkeypatch, title="A Paper From Yesterday Nobody Has Indexed", search=None)
    out = await pg.paper_graph("https://arxiv.org/abs/2609.01234")
    assert out["paper"] is None
    assert "arXiv calls it 'A Paper From Yesterday Nobody Has Indexed'" in out["notes"][0]
    assert "may not be indexed yet" in out["notes"][0]


async def test_a_failing_arxiv_lookup_never_raises(monkeypatch):
    async def boom(url, **kw):
        raise RuntimeError("arxiv is down")

    monkeypatch.setattr(pg._api, "_get_json", _Api())
    monkeypatch.setattr(pg._api, "_request_text", boom)
    out = await pg.paper_graph("arXiv:1706.03762")
    assert out["paper"] is None and out["notes"]


async def test_among_records_with_the_exact_arxiv_title_the_most_cited_wins(monkeypatch):
    """OpenAlex can hold several records under one exact title — a preprint
    and its proceedings version. The citation graph hangs off the cited one,
    and a merely similar title must not win on citations alone."""
    redeposit = dict(_TARGET, id="https://openalex.org/W1", cited_by_count=12)
    canonical = dict(_TARGET, id="https://openalex.org/W2", cited_by_count=90_000)
    other = dict(_TARGET, id="https://openalex.org/W3", cited_by_count=999_999,
                 display_name="Reciprocal rank fusion in speech separation")  # fmt: skip

    class Api(_Api):
        async def __call__(self, url, **kw):
            if "?search=" in url:
                self.urls.append(url)
                return {"results": [redeposit, other, canonical]}
            return await super().__call__(url, **kw)

    async def text(url, **kw):
        return _ARXIV_FEED.format(title=_TARGET["display_name"])

    monkeypatch.setattr(pg._api, "_get_json", Api())
    monkeypatch.setattr(pg._api, "_request_text", text)

    out = await pg.paper_graph("arXiv:1706.03762", direction="references")

    assert out["paper"]["openalex_id"] == "W2"
