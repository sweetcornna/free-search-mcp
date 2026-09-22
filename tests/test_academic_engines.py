"""Scholarly engines (offline).

Grouped in one module the way `test_parse_default_engines.py` groups the
default HTML scrapers: they all share the `JsonApiEngine` base, so testing them
together keeps the payload fixtures next to each other.

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
from search_mcp.engines.clinicaltrials import ClinicalTrialsEngine
from search_mcp.engines.crossref import CrossrefEngine
from search_mcp.engines.dblp import DblpEngine
from search_mcp.engines.doaj import DoajEngine
from search_mcp.engines.europepmc import EuropePmcEngine
from search_mcp.engines.openalex import OpenAlexEngine
from search_mcp.engines.pubmed import PubMedEngine
from search_mcp.engines.semanticscholar import SemanticScholarEngine

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.

NETWORK = os.environ.get("SEARCH_MCP_TEST_NETWORK") == "1"
skip_offline = pytest.mark.skipif(
    not NETWORK, reason="set SEARCH_MCP_TEST_NETWORK=1 to run"
)

ACADEMIC = [
    "arxiv",
    "openalex",
    "crossref",
    "pubmed",
    "europepmc",
    "semanticscholar",
    "dblp",
    "doaj",
    "clinicaltrials",
    "zbmath",
]


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
    # The exact phrase, OR every content word. Stopwords are dropped from the
    # AND side because arXiv does not index them: one `all:is` and the whole
    # conjunction matches nothing.
    assert (
        "search_query=all:%22attention+is+all+you+need%22"
        "+OR+%28all:attention+AND+all:need%29"
    ) in url
    assert "max_results=5" in url
    # arXiv treats 0 as "no results" and rejects huge values.
    assert "max_results=1" in e.build_url("x", 0)
    assert "max_results=100" in e.build_url("x", 9999)


def test_arxiv_query_shapes():
    from search_mcp.engines.arxiv import _search_query

    # Every content word must match — not "any word", which is what a bare
    # `all:a b c` means to arXiv and why "rank fusion" found image fusion.
    assert _search_query("reciprocal rank fusion") == (
        "all:%22reciprocal+rank+fusion%22+OR+%28all:reciprocal+AND+all:rank+AND+all:fusion%29"
    )
    # Short acronyms are content, not noise.
    assert "all:GAN+AND+all:LLM" in _search_query("GAN vs LLM")
    # One word, or a caller-written phrase expression: passed through.
    assert _search_query("transformers") == "all:transformers"
    assert _search_query('"residual learning" imagenet') == "all:%22residual+learning%22+imagenet"
    # Nothing but stopwords left: fall back rather than send an empty AND.
    assert _search_query("to be or not") == "all:to+be+or+not"
    # A long description is capped so the conjunction stays satisfiable.
    long = _search_query("one two three four five six seven eight nine ten")
    assert long.count("+AND+") == 7


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
            {
                "title": ["Year And A Gap"],
                "URL": "https://doi.org/10.1/g",
                "issued": {"date-parts": [[2024, None, 5]]},
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


def test_crossref_reports_dates_at_their_real_precision():
    """A full Y-M-D is confident; a year-only record is NOT.

    Year-only `issued` is extremely common. Padding it to `2024-01-01` and
    flagging it confident let the freshness filter drop a paper actually issued
    in December on the strength of a date Crossref never published — and showed
    the reader a day the paper does not have. Open Library already takes this
    line for its year-only dates.
    """
    out = CrossrefEngine().map_results(_CROSSREF)
    assert out[0].published_age == "2024-02-05"
    assert out[0].published_age_confident is True
    assert out[1].published_age == "2024"
    assert out[1].published_age_confident is False


def test_crossref_truncates_date_parts_at_the_first_gap():
    """`date-parts` is positional: `[[2024, None, 5]]` means "2024, month
    unknown". Filtering the Nones out instead of stopping at the first one
    collapsed the list to `[2024, 5]` and promoted the DAY into the month
    slot, reporting `2024-05-01`."""
    out = CrossrefEngine().map_results(_CROSSREF)
    assert out[3].title == "Year And A Gap"
    assert out[3].published_age == "2024"
    assert out[3].published_age_confident is False


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
    assert len(out) == 4


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
# Europe PMC
# ---------------------------------------------------------------------------

_EUROPEPMC = {
    "hitCount": 228334,
    "resultList": {
        "result": [
            {
                "id": "38270601",
                "source": "MED",
                "pmid": "38270601",
                "pmcid": "PMC10871810",
                "doi": "10.1093/nar/gkae023",
                "title": "CRISPR-Cas9 screening of proteins.",
                "authorString": "Zhang Y, Liu Q, Chen X.",
                "journalInfo": {"journal": {"title": "Nucleic Acids Res"}},
                "abstractText": "We report a genome-wide screen.",
                "firstPublicationDate": "2024-01-25",
                "citedByCount": 42,
                "isOpenAccess": "Y",
                "fullTextUrlList": {
                    "fullTextUrl": [
                        {
                            "availabilityCode": "OA",
                            "documentStyle": "pdf",
                            "url": "https://europepmc.org/articles/PMC10871810?pdf=render",
                        },
                        {
                            "availabilityCode": "OA",
                            "documentStyle": "html",
                            "url": "https://europepmc.org/articles/PMC10871810",
                        },
                    ]
                },
            },
            {
                "id": "PPR812345",
                "source": "PPR",
                "doi": "10.1101/2026.01.01.000000",
                "title": "A preprint about base editors",
                "authorString": "Doe J.",
                "firstPublicationDate": "2026-01-01",
            },
            {"id": "x", "source": "MED"},  # no title
        ]
    },
}


def test_europepmc_prefers_open_access_html_over_the_pdf():
    """An HTML full text is what `read_doc` can actually open cheaply; the PDF
    is the same content behind a much heavier fetch."""
    out = EuropePmcEngine().map_results(_EUROPEPMC)
    assert out[0].url == "https://europepmc.org/articles/PMC10871810"


def test_europepmc_falls_back_to_the_record_page():
    out = EuropePmcEngine().map_results(_EUROPEPMC)
    assert out[1].url == "https://europepmc.org/article/PPR/PPR812345"


def test_europepmc_flags_preprints_in_the_snippet():
    """A preprint has not been peer reviewed, and a model quoting it must be
    able to say so without opening the record."""
    out = EuropePmcEngine().map_results(_EUROPEPMC)
    assert out[1].snippet.startswith("PREPRINT")
    assert "PREPRINT" not in out[0].snippet


def test_europepmc_snippet_carries_journal_citations_and_access():
    out = EuropePmcEngine().map_results(_EUROPEPMC)
    assert "Nucleic Acids Res" in out[0].snippet
    assert "cited by 42" in out[0].snippet
    assert "open access" in out[0].snippet


def test_europepmc_dates_are_confident():
    out = EuropePmcEngine().map_results(_EUROPEPMC)
    assert out[0].published_age == "2024-01-25"
    assert out[0].published_age_confident is True


def test_europepmc_skips_records_without_a_title():
    assert len(EuropePmcEngine().map_results(_EUROPEPMC)) == 2


@pytest.mark.parametrize(
    ("token", "clause"),
    [
        ("paper.preprint", '%28SRC%3A%22PPR%22%29'),
        ("paper.openaccess", '%28OPEN_ACCESS%3A%22Y%22%29'),
    ],
)
def test_europepmc_sub_group_adds_an_index_side_clause(token, clause):
    """The sub-group restriction is Europe PMC's own field syntax, so the index
    does the narrowing — filtering here would return a page of the wrong hits
    and then throw most of it away. bioRxiv's own API cannot keyword-search at
    all, which is why preprint search runs through this clause."""
    url = EuropePmcEngine().build_url(
        "crispr", 10, SearchFilters(category="paper", category_token=token)
    )
    assert clause in url


def test_europepmc_bare_group_adds_no_clause():
    url = EuropePmcEngine().build_url(
        "crispr", 10, SearchFilters(category="paper", category_token="paper")
    )
    assert "SRC" not in url and "OPEN_ACCESS" not in url


def test_europepmc_freshness_becomes_a_date_range_clause():
    url = EuropePmcEngine().build_url("crispr", 10, SearchFilters(freshness="week"))
    assert "FIRST_PDATE" in url


# ---------------------------------------------------------------------------
# Semantic Scholar
# ---------------------------------------------------------------------------

_S2 = {
    "total": 1234,
    "data": [
        {
            "paperId": "204e3073870fae3d05bcbc2f6a8e263d9b72e776",
            "title": "Attention Is All You Need",
            "abstract": "The dominant sequence transduction models…",
            "year": 2017,
            "publicationDate": "2017-06-12",
            "venue": "NeurIPS",
            "citationCount": 100000,
            "influentialCitationCount": 12000,
            "isOpenAccess": True,
            "openAccessPdf": {"url": "https://arxiv.org/pdf/1706.03762"},
            "externalIds": {"DOI": "10.5555/3295222.3295349"},
            "authors": [
                {"name": "Ashish Vaswani"},
                {"name": "Noam Shazeer"},
                {"name": "Niki Parmar"},
                {"name": "Jakob Uszkoreit"},
            ],
        },
        {
            "paperId": "abc",
            "title": "Year only",
            "year": 2019,
            "externalIds": {"DOI": "10.1000/xyz"},
        },
    ],
}


def test_semanticscholar_stays_out_of_auto_routing_without_a_key(monkeypatch):
    """The anonymous pool answered 429 on every attempt while this engine was
    written. Spending one of `category_engine_limit`'s slots on a guaranteed
    empty costs a real source — but naming it explicitly still runs it."""
    from search_mcp.engines import semanticscholar as mod

    monkeypatch.setattr(mod, "get_secret", lambda field: "")
    assert SemanticScholarEngine().is_available() is False
    monkeypatch.setattr(mod, "get_secret", lambda field: "k")
    assert SemanticScholarEngine().is_available() is True


def test_semanticscholar_sends_the_key_as_a_header(monkeypatch):
    from search_mcp.engines import semanticscholar as mod

    monkeypatch.setattr(mod, "get_secret", lambda field: "sekrit")
    assert SemanticScholarEngine().api_headers == {"x-api-key": "sekrit"}
    monkeypatch.setattr(mod, "get_secret", lambda field: "")
    assert SemanticScholarEngine().api_headers == {}


def test_semanticscholar_prefers_the_open_access_pdf():
    out = SemanticScholarEngine().map_results(_S2)
    assert out[0].url == "https://arxiv.org/pdf/1706.03762"
    assert out[1].url == "https://doi.org/10.1000/xyz"


def test_semanticscholar_snippet_reports_influential_citations():
    out = SemanticScholarEngine().map_results(_S2)
    assert "cited by 100000 (12000 influential)" in out[0].snippet
    assert "Ashish Vaswani, Noam Shazeer, Niki Parmar et al." in out[0].snippet


def test_semanticscholar_year_only_is_shown_but_not_trusted():
    """Same convention as OpenLibrary and (since this release) Crossref: a bare
    year is not precise enough for `freshness` to drop a result on."""
    out = SemanticScholarEngine().map_results(_S2)
    assert out[0].published_age == "2017-06-12"
    assert out[0].published_age_confident is True
    assert out[1].published_age == "2019"
    assert out[1].published_age_confident is False


def test_semanticscholar_asks_for_the_fields_it_renders():
    url = SemanticScholarEngine().build_url("attention", 5)
    for field in ("abstract", "citationCount", "openAccessPdf", "externalIds"):
        assert field in url


# ---------------------------------------------------------------------------
# DBLP
# ---------------------------------------------------------------------------

_DBLP = {
    "result": {
        "hits": {
            "@total": "27",
            "hit": [
                {
                    "info": {
                        "authors": {
                            "author": [
                                {"@pid": "1", "text": "Gordon V. Cormack"},
                                {"@pid": "2", "text": "Charles L. A. Clarke"},
                                {"@pid": "3", "text": "Stefan Büttcher"},
                            ]
                        },
                        "title": "Reciprocal rank fusion outperforms Condorcet.",
                        "venue": "SIGIR",
                        "year": "2009",
                        "type": "Conference and Workshop Papers",
                        "doi": "10.1145/1571941.1572114",
                        "ee": "https://doi.org/10.1145/1571941.1572114",
                        "url": "https://dblp.org/rec/conf/sigir/CormackCB09",
                    }
                },
                {
                    "info": {
                        # A single-author paper serialises `author` as an OBJECT.
                        "authors": {"author": {"@pid": "9", "text": "Solo Writer"}},
                        "title": "One author only",
                        "year": "2021",
                        "url": "https://dblp.org/rec/conf/x/Writer21",
                    }
                },
            ],
        }
    }
}


def test_dblp_handles_a_single_author_serialised_as_an_object():
    """DBLP's JSON is generated from XML, so a one-author record collapses the
    list into a bare object — the classic XML-to-JSON trap."""
    assert DblpEngine()._authors(_DBLP["result"]["hits"]["hit"][1]["info"]) == [
        "Solo Writer"
    ]


def test_dblp_links_the_electronic_edition_not_the_record_page():
    """`ee` is usually the DOI, i.e. the paper itself; the DBLP record is one
    hop further away."""
    out = DblpEngine().map_results(_DBLP)
    assert out[0].url == "https://doi.org/10.1145/1571941.1572114"
    assert out[1].url == "https://dblp.org/rec/conf/x/Writer21"


def test_dblp_snippet_names_the_venue_and_year():
    out = DblpEngine().map_results(_DBLP)
    assert "SIGIR" in out[0].snippet and "2009" in out[0].snippet
    assert out[0].title == "Reciprocal rank fusion outperforms Condorcet"


def test_dblp_year_is_never_confident():
    """DBLP records a year and nothing finer, so `freshness` must not drop on
    it — the same rule the Crossref year-only fix established."""
    out = DblpEngine().map_results(_DBLP)
    assert out[0].published_age == "2009"
    assert out[0].published_age_confident is False


def test_dblp_declares_a_gentle_rate_limit_it_will_not_wait_on():
    """Two quick queries during development came back throttled — as an EMPTY
    body, which the never-raise rule turns into "no results"."""
    engine = DblpEngine()
    assert engine.rate_limit_per_minute == 20
    assert engine.rate_limit_max_wait == 3.0


# ---------------------------------------------------------------------------
# DOAJ
# ---------------------------------------------------------------------------

_DOAJ = {
    "total": 4021,
    "results": [
        {
            "id": "0a1b2c3d",
            "bibjson": {
                "title": "Machine learning for\n  soil mapping",
                "abstract": "We compare random forests and gradient boosting.",
                "year": "2023",
                "month": "4",
                "author": [{"name": "A. Author"}, {"name": "B. Author"}],
                "journal": {"title": "Open Soil Science"},
                "identifier": [
                    {"type": "doi", "id": "10.1234/oss.2023.1"},
                    {"type": "eissn", "id": "1234-5678"},
                ],
                "link": [
                    {"type": "fulltext", "url": "https://journal.example/article/1"}
                ],
            },
        },
        {
            "id": "deadbeef",
            "bibjson": {
                "title": "No full text link",
                "year": "2020",
                "identifier": [{"type": "doi", "id": "10.1234/x"}],
            },
        },
    ],
}


def test_doaj_query_is_path_encoded():
    """The query lives in the URL PATH, so an unescaped "/" would change which
    endpoint is called."""
    url = DoajEngine().build_url("a/b c", 5)
    assert url.startswith("https://doaj.org/api/search/articles/a%2Fb%20c?")


def test_doaj_links_the_publisher_full_text_then_the_doi():
    out = DoajEngine().map_results(_DOAJ)
    assert out[0].url == "https://journal.example/article/1"
    assert out[1].url == "https://doi.org/10.1234/x"


def test_doaj_collapses_whitespace_in_titles():
    out = DoajEngine().map_results(_DOAJ)
    assert out[0].title == "Machine learning for soil mapping"


def test_doaj_year_month_is_shown_but_not_trusted():
    out = DoajEngine().map_results(_DOAJ)
    assert out[0].published_age == "2023-04"
    assert out[0].published_age_confident is False
    assert out[1].published_age == "2020"


# ---------------------------------------------------------------------------
# ClinicalTrials.gov
# ---------------------------------------------------------------------------

_TRIALS = {
    "totalCount": 312,
    "studies": [
        {
            "protocolSection": {
                "identificationModule": {
                    "nctId": "NCT03548935",
                    "briefTitle": "Semaglutide Effects on Body Weight",
                },
                "statusModule": {
                    "overallStatus": "ACTIVE_NOT_RECRUITING",
                    "startDateStruct": {"date": "2018-06-04", "type": "ACTUAL"},
                },
                "designModule": {
                    "phases": ["PHASE3"],
                    "enrollmentInfo": {"count": 1961, "type": "ACTUAL"},
                },
                "conditionsModule": {"conditions": ["Obesity", "Overweight"]},
                "sponsorCollaboratorsModule": {"leadSponsor": {"name": "Novo Nordisk"}},
                "descriptionModule": {"briefSummary": "This study looks at weight."},
            }
        },
        # No identification module: nothing to link to or name.
        {"protocolSection": {"statusModule": {"overallStatus": "COMPLETED"}}},
    ],
}


def test_clinicaltrials_titles_carry_the_nct_id():
    """The registry ID is how a trial is cited everywhere else, so it belongs
    in the title rather than only in the URL."""
    out = ClinicalTrialsEngine().map_results(_TRIALS)
    assert len(out) == 1
    assert out[0].title == "NCT03548935 — Semaglutide Effects on Body Weight"
    assert out[0].url == "https://clinicaltrials.gov/study/NCT03548935"


def test_clinicaltrials_snippet_leads_with_phase_status_and_enrolment():
    """"What stage is this drug at" is usually the actual question."""
    out = ClinicalTrialsEngine().map_results(_TRIALS)
    assert out[0].snippet.startswith(
        "Phase 3 · Active Not Recruiting · n=1961 · Obesity, Overweight · Novo Nordisk"
    )


def test_clinicaltrials_start_date_is_confident():
    out = ClinicalTrialsEngine().map_results(_TRIALS)
    assert out[0].published_age == "2018-06-04"
    assert out[0].published_age_confident is True


def test_clinicaltrials_requests_only_the_modules_it_renders():
    """The default study record is tens of KB per hit — every outcome measure
    and every site's contact details."""
    url = ClinicalTrialsEngine().build_url("obesity", 5)
    assert "fields=protocolSection.identificationModule" in url
    assert "eligibilityModule" not in url


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
        ("europepmc", "crispr gene editing"),
        ("dblp", "reciprocal rank fusion"),
        ("doaj", "machine learning"),
        ("clinicaltrials", "semaglutide obesity"),
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
