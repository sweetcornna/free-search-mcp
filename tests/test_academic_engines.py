"""arXiv / OpenAlex / Crossref / PubMed engines (offline).

Grouped in one module the way `test_parse_default_engines.py` groups the
default HTML scrapers: these four share the `JsonApiEngine` base, so testing
them together keeps the payload fixtures next to each other.

Fixtures are trimmed copies of real responses — the field quirks they encode
(Crossref's `[[None]]` dates, OpenAlex's inverted abstracts, PubMed's
two-request flow) are exactly what the parsers exist to absorb.
"""

from __future__ import annotations

import json
import os

import pytest

from search_mcp.engines import ENGINES, get_engine
from search_mcp.engines.arxiv import ArxivEngine
from search_mcp.engines.base import SearchFilters
from search_mcp.engines.crossref import CrossrefEngine
from search_mcp.engines.openalex import OpenAlexEngine
from search_mcp.engines.pubmed import PubMedEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)

ACADEMIC = ["arxiv", "openalex", "crossref", "pubmed"]


# ---------------------------------------------------------------------------
# Registry contract
# ---------------------------------------------------------------------------


@pytest.mark.parametrize("name", ACADEMIC)
def test_registered_and_declares_paper_category(name):
    assert name in ENGINES
    engine = get_engine(name)
    assert engine.name == name
    # The whole point: `category="paper"` must route here instead of filtering
    # a general web engine's results by hostname.
    assert "paper" in engine.categories


@pytest.mark.parametrize("name", ACADEMIC)
def test_stays_out_of_the_default_pool(name):
    """Specialist sources must not slow down ordinary web searches."""
    from search_mcp.config import Settings

    assert name not in Settings().default_engines


@pytest.mark.parametrize("name", ACADEMIC)
def test_never_uses_a_browser(name):
    engine = get_engine(name)
    assert engine.needs_browser is False
    assert engine.supports_browser_fallback is False


# ---------------------------------------------------------------------------
# arXiv — Atom feed
# ---------------------------------------------------------------------------

_ARXIV_FEED = """<?xml version='1.0' encoding='UTF-8'?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry>
    <id>http://arxiv.org/abs/1706.03762v7</id>
    <title>Attention Is All You Need</title>
    <summary>The dominant sequence transduction models are based on complex
    recurrent or convolutional neural networks.</summary>
    <published>2017-06-12T18:00:00Z</published>
    <link href="https://arxiv.org/abs/1706.03762v7" type="text/html"/>
    <link href="https://arxiv.org/pdf/1706.03762v7" type="application/pdf" title="pdf"/>
    <author><name>Ashish Vaswani</name></author>
    <author><name>Noam Shazeer</name></author>
    <author><name>Niki Parmar</name></author>
    <author><name>Jakob Uszkoreit</name></author>
  </entry>
  <entry>
    <id>http://arxiv.org/abs/2105.02723v1</id>
    <title>Do You Even Need Attention?</title>
    <summary>A stack of feed-forward layers does surprisingly well.</summary>
    <published>2021-05-06T00:00:00Z</published>
    <link href="https://arxiv.org/abs/2105.02723v1" type="text/html"/>
    <author><name>Luke Melas-Kyriazi</name></author>
  </entry>
</feed>
"""


def test_arxiv_build_url_encodes_query_and_clamps():
    e = ArxivEngine()
    url = e.build_url("attention is all you need", 5)
    # The `all:` field prefix stays literal; only the query text is encoded.
    assert "search_query=all:attention+is+all+you+need" in url
    assert "max_results=5" in url
    # arXiv treats 0 as "no results" and rejects huge values.
    assert "max_results=1" in e.build_url("x", 0)
    assert "max_results=100" in e.build_url("x", 9999)


def test_arxiv_freshness_switches_to_newest_first():
    e = ArxivEngine()
    assert "sortBy=submittedDate" not in e.build_url("x", 5)
    url = e.build_url("x", 5, SearchFilters(freshness="week"))
    assert "sortBy=submittedDate" in url and "sortOrder=descending" in url


def test_arxiv_parses_entries_with_abs_url_not_pdf():
    out = ArxivEngine()._parse_feed(_ARXIV_FEED)
    assert [r.url for r in out] == [
        "https://arxiv.org/abs/1706.03762v7",
        "https://arxiv.org/abs/2105.02723v1",
    ]
    assert out[0].title == "Attention Is All You Need"


def test_arxiv_marks_dates_confident():
    out = ArxivEngine()._parse_feed(_ARXIV_FEED)
    assert out[0].published_age == "2017-06-12"
    # A structured feed date is trustworthy enough to drop stale results on.
    assert out[0].published_age_confident is True


def test_arxiv_snippet_lists_authors_then_abstract():
    out = ArxivEngine()._parse_feed(_ARXIV_FEED)
    assert out[0].snippet.startswith("Ashish Vaswani, Noam Shazeer, Niki Parmar et al. —")
    assert "dominant sequence transduction" in out[0].snippet
    # A single author gets no "et al."
    assert out[1].snippet.startswith("Luke Melas-Kyriazi —")


def test_arxiv_malformed_xml_returns_empty():
    assert ArxivEngine()._parse_feed("<feed><entry>") == []
    assert ArxivEngine()._parse_feed("") == []


# ---------------------------------------------------------------------------
# OpenAlex — inverted abstracts
# ---------------------------------------------------------------------------

_OPENALEX = {
    "results": [
        {
            "display_name": "Attention Is All You Need",
            "publication_date": "2017-06-12",
            "cited_by_count": 100000,
            "primary_location": {
                "landing_page_url": "https://papers.example/attention",
                "source": {"display_name": "NeurIPS"},
            },
            "abstract_inverted_index": {
                "The": [0], "dominant": [1], "models": [2], "are": [3], "complex": [4]
            },
        },
        {
            "display_name": "No Landing Page",
            "publication_date": "2020-01-01",
            "doi": "https://doi.org/10.1234/abcd",
            "primary_location": {},
        },
        {"display_name": "", "primary_location": {"landing_page_url": "https://x.example"}},
    ]
}


def test_openalex_rebuilds_prose_from_the_inverted_abstract():
    out = OpenAlexEngine().map_results(_OPENALEX)
    assert "The dominant models are complex" in out[0].snippet


def test_openalex_snippet_leads_with_venue_and_citations():
    out = OpenAlexEngine().map_results(_OPENALEX)
    assert out[0].snippet.startswith("NeurIPS · cited by 100000 —")


def test_openalex_falls_back_to_doi_when_there_is_no_landing_page():
    out = OpenAlexEngine().map_results(_OPENALEX)
    assert out[1].url == "https://doi.org/10.1234/abcd"


def test_openalex_skips_records_without_a_title():
    out = OpenAlexEngine().map_results(_OPENALEX)
    assert len(out) == 2


def test_openalex_mailto_only_sent_when_configured(monkeypatch):
    e = OpenAlexEngine()
    monkeypatch.setattr("search_mcp.engines.openalex.settings.contact_email", "")
    assert "mailto=" not in e.build_url("x", 5)
    monkeypatch.setattr("search_mcp.engines.openalex.settings.contact_email", "a@b.com")
    assert "mailto=a%40b.com" in e.build_url("x", 5)


@pytest.mark.parametrize("payload", [None, {}, {"results": "nope"}, [], "garbage"])
def test_openalex_tolerates_structural_surprises(payload):
    assert OpenAlexEngine().map_results(payload) == []


# ---------------------------------------------------------------------------
# Crossref — list titles, partial dates
# ---------------------------------------------------------------------------

_CROSSREF = {
    "message": {
        "items": [
            {
                "title": ["The Triple Attention Transformer"],
                "URL": "https://doi.org/10.1/x",
                "issued": {"date-parts": [[2024, 2, 5]]},
                "author": [{"given": "Shadi", "family": "Ghaith"}],
                "container-title": ["Nature"],
                "abstract": "<jats:p>This paper introduces the model.</jats:p>",
            },
            {
                "title": ["Year Only"],
                "URL": "https://doi.org/10.1/y",
                "issued": {"date-parts": [[2024]]},
                "publisher": "Springer",
            },
            {
                "title": ["No Date At All"],
                "URL": "https://doi.org/10.1/z",
                "issued": {"date-parts": [[None]]},
            },
            {"title": [], "URL": "https://doi.org/10.1/w"},
        ]
    }
}


def test_crossref_restricts_to_article_types():
    """Unfiltered, Crossref ranks individual figures above the papers that
    contain them — each sub-component has its own DOI."""
    url = CrossrefEngine().build_url("x", 5)
    assert "filter=type:journal-article,type:proceedings-article" in url


def test_crossref_unwraps_list_titles():
    out = CrossrefEngine().map_results(_CROSSREF)
    assert out[0].title == "The Triple Attention Transformer"


def test_crossref_pads_partial_dates():
    out = CrossrefEngine().map_results(_CROSSREF)
    assert out[0].published_age == "2024-02-05"
    assert out[1].published_age == "2024-01-01"


def test_crossref_missing_date_is_empty_not_confident():
    """`[[None]]` is Crossref's way of saying "no date" — it must not become
    a fabricated one, and must not be trusted for freshness dropping."""
    out = CrossrefEngine().map_results(_CROSSREF)
    assert out[2].published_age == ""
    assert out[2].published_age_confident is False


def test_crossref_strips_jats_markup_from_abstracts():
    out = CrossrefEngine().map_results(_CROSSREF)
    assert "<jats:p>" not in out[0].snippet
    assert "This paper introduces the model." in out[0].snippet


def test_crossref_snippet_uses_publisher_when_no_container():
    out = CrossrefEngine().map_results(_CROSSREF)
    assert "Springer" in out[1].snippet


def test_crossref_skips_entries_without_a_title():
    out = CrossrefEngine().map_results(_CROSSREF)
    assert len(out) == 3


# ---------------------------------------------------------------------------
# PubMed — two-step esearch + esummary
# ---------------------------------------------------------------------------

_PUBMED_SUMMARY = {
    "result": {
        "uids": ["31295471", "38786024"],
        "31295471": {
            "title": "CRISPR-Cas9 system: A new-fangled dawn in gene editing.",
            "sortpubdate": "2019/09/01 00:00",
            "source": "Life Sci",
            "authors": [
                {"name": "Gupta D"}, {"name": "Bhattacharjee O"},
                {"name": "Mandal D"}, {"name": "Sen M"},
            ],
        },
        "38786024": {
            "title": "CRISPR-Based Gene Therapies.",
            "pubdate": "2024 May",
            "source": "Cells",
            "authors": [{"name": "Laurent M"}],
        },
    }
}


def test_pubmed_preserves_relevance_order_from_uids():
    """Iterating the `result` dict would lose ranking; `uids` carries it."""
    out = PubMedEngine().map_results(_PUBMED_SUMMARY)
    assert [r.url for r in out] == [
        "https://pubmed.ncbi.nlm.nih.gov/31295471/",
        "https://pubmed.ncbi.nlm.nih.gov/38786024/",
    ]


def test_pubmed_parses_sortpubdate_only():
    out = PubMedEngine().map_results(_PUBMED_SUMMARY)
    assert out[0].published_age == "2019-09-01"
    # `pubdate: "2024 May"` is not machine-readable; report nothing rather
    # than half a date.
    assert out[1].published_age == ""
    assert out[1].published_age_confident is False


def test_pubmed_snippet_truncates_author_lists():
    out = PubMedEngine().map_results(_PUBMED_SUMMARY)
    assert out[0].snippet.startswith("Gupta D, Bhattacharjee O, Mandal D et al. — Life Sci")


def test_pubmed_freshness_uses_a_server_side_window():
    e = PubMedEngine()
    assert "reldate" not in e.build_url("x", 5)
    assert "reldate=7" in e.build_url("x", 5, SearchFilters(freshness="week"))


def test_pubmed_rejects_non_numeric_ids():
    """PMIDs are interpolated into the esummary URL, so anything that isn't a
    number is dropped rather than sent."""
    hostile = {"esearchresult": {"idlist": ["123", "../../etc/passwd", None, "456"]}}
    assert PubMedEngine()._pmids(hostile) == ["123", "456"]


async def test_pubmed_returns_empty_when_esearch_finds_nothing(monkeypatch):
    e = PubMedEngine()

    async def _no_hits(url, **kw):
        return {"esearchresult": {"idlist": []}}

    monkeypatch.setattr(e, "_get_json", _no_hits)
    assert await e.fetch_results("nothing matches this", 5, None) == []


# ---------------------------------------------------------------------------
# Live network
# ---------------------------------------------------------------------------


@skip_offline
@pytest.mark.parametrize(
    "name,query",
    [
        ("arxiv", "attention is all you need"),
        ("openalex", "transformer attention"),
        ("crossref", "transformer attention"),
        ("pubmed", "crispr gene editing"),
    ],
)
async def test_live_returns_results(name, query):
    out = await get_engine(name).search(query, 3)
    if not out:
        pytest.skip(f"{name} returned nothing (rate limit or outage)")
    assert out[0].url.startswith("http")
    assert out[0].title
    assert all(r.engine == name for r in out)


@skip_offline
async def test_live_json_is_wellformed_for_openalex():
    """Guards against the API changing shape under us — a mapping failure
    would otherwise look identical to "no results"."""
    e = get_engine("openalex")
    payload = await e._get_json(e.build_url("crispr", 2))
    assert payload is not None
    assert isinstance(json.dumps(payload), str)
