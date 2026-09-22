"""Citation-graph traversal for a single paper: references, citations, retraction.

Search finds papers that MENTION your words. The citation graph finds the
papers a given work actually built on and the papers that actually built on it
— a different question, and the one you ask once you have a paper in hand:

  * backward (`references`) — what this work stands on. The reading list.
  * forward (`citations`) — what has been done since, ordered by how much the
    field cited it. This is how you find the state of the art from a paper
    that is five years old, and how you notice that the result was superseded.
  * retraction — whether the paper is still standing at all.

Two keyless APIs, both already used by registered engines:

  * OpenAlex resolves the work and holds both edge directions. Backward edges
    live IN the record (`referenced_works`, a list of OpenAlex IDs) and are
    restored with one batched `filter=openalex_id:W1|W2|…` call; forward edges
    are a query, `filter=cites:<id>&sort=cited_by_count:desc`.
  * Crossref answers the verification question OpenAlex cannot: whether the DOI
    is REGISTERED, and what has been published about the paper since —
    `updated-by` carries retraction notices, corrections and expressions of
    concern, with the notice's own DOI and date.

Cost is bounded and small: at most four HTTP requests regardless of `limit`
(resolve, one batched reference restore, one citations page, one Crossref
lookup), because every neighbour's metadata is selected in the same call that
lists it.
"""

from __future__ import annotations

import html
import logging
import re
from typing import Any
from urllib.parse import quote, quote_plus

from rapidfuzz import fuzz, utils

from .config import settings
from .engines.base import SearchResult
from .engines.jsonapi import JsonApiEngine, _last_http_status, clip

log = logging.getLogger(__name__)

_WORKS = "https://api.openalex.org/works"
_CROSSREF = "https://api.crossref.org/works"

# Everything the renderer shows, and nothing else — the full OpenAlex work
# record is ~17 KB apiece, and this tool asks for up to 2×`limit` of them.
_SELECT = ",".join(
    (
        "id",
        "doi",
        "display_name",
        "publication_year",
        "cited_by_count",
        "primary_location",
        "is_retracted",
    )
)
# The target additionally needs its backward edges and its author list.
_SELECT_TARGET = _SELECT + ",referenced_works,authorships"

# OpenAlex accepts an OR list in a filter; 50 is its documented ceiling. Larger
# reference lists are truncated rather than paged — this is a neighbourhood
# view, not an export.
_MAX_BATCH = 50
_MAX_LIMIT = 50

# How many candidates a title lookup considers before deciding, and how close
# the best one has to be to count as "this is the paper you meant".
#
# Getting this wrong in the permissive direction is the worst thing this tool
# can do: presenting FAD-BERT's citation graph under the heading a caller typed
# for BERT is a confidently wrong answer, and a caller checking a citation has
# no way to catch it. Refusing is safe — the candidates come back in `notes`,
# so a retry with a DOI is one step away.
_TITLE_CANDIDATES = 5
_TITLE_MIN = 85.0
# "The same title", allowing for case and punctuation. Only used when the query
# title came from a registry rather than from someone's memory.
_EXACT_TITLE = 97.0

# Post-publication notices, most severe first. Crossref files the SAME notice
# DOI under several types — the Lancet Surgisphere paper lists one notice as
# both `retraction` and `erratum` — so the list is deduplicated by notice DOI
# keeping the most serious reading, then ordered by this rank. Unranked types
# sort last but are still shown; the vocabulary is open-ended.
_NOTICE_RANK = {
    "retraction": 0,
    "withdrawal": 1,
    "removal": 1,
    "expression_of_concern": 2,
    "correction": 3,
    "corrigendum": 3,
    "erratum": 4,
    "addendum": 5,
}
_MAX_NOTICES = 6

# A DOI is `10.<registrant>/<suffix>`; anything else is treated as a title.
_DOI_RE = re.compile(r"\b(10\.\d{4,9}/\S+)", re.I)
# `W` + digits, optionally as a full openalex.org URL.
_OPENALEX_RE = re.compile(r"\b(W\d{4,12})\b")

# arXiv identifiers, in the three forms a model has on hand: the DataCite DOI
# arXiv mints for every preprint (`10.48550/arXiv.1706.03762`), an abs/pdf URL,
# and the `arXiv:` citation form. New-style ids (`1706.03762`) and old-style
# ones (`hep-th/9901001`), with an optional version suffix that is dropped.
_ARXIV_ID = r"([a-z-]+(?:\.[a-z]{2})?/\d{7}|\d{4}\.\d{4,5})(?:v\d+)?"
_ARXIV_DOI_RE = re.compile(r"^10\.48550/arxiv\." + _ARXIV_ID + r"$", re.I)
_ARXIV_REF_RE = re.compile(r"(?:arxiv\.org/(?:abs|pdf)/|\barxiv:\s*)" + _ARXIV_ID, re.I)
# With no prefix to mark it, an id is trusted only as the WHOLE input:
# "1706.03762" inside a title is a number, not an identifier.
_ARXIV_BARE_RE = re.compile(r"^" + _ARXIV_ID + r"$", re.I)
_ARXIV_API = "https://export.arxiv.org/api/query"
_ATOM_ENTRY_TITLE_RE = re.compile(r"<entry>.*?<title>(.*?)</title>", re.S)

Direction = str  # "both" | "references" | "citations"


class _GraphApi(JsonApiEngine):
    """Transport only — deliberately NOT registered in `ENGINES`.

    Subclassing buys the shared curl_cffi session, the proxy settings, the
    timeout and the never-raise boundary that every other JSON source in this
    package already uses, without inventing a second HTTP path. It is not a
    search engine and never appears in `engines()`.
    """

    name = "paper_graph"
    description = "internal transport for the paper_graph tool"

    def build_url(self, query: str, max_results: int, filters: Any = None) -> str:
        return _WORKS

    def map_results(self, payload: Any) -> list[SearchResult]:
        return []


_api = _GraphApi()


def _mailto() -> str:
    """OpenAlex's faster "polite pool" is opt-in by contact address."""
    return f"&mailto={quote_plus(settings.contact_email)}" if settings.contact_email else ""


def _normalize(paper: str) -> tuple[str, str]:
    """Classify the identifier: `("doi"|"openalex"|"title", value)`.

    Accepts what a model actually has on hand — a bare DOI, a doi.org URL, a
    `doi:` prefix, an OpenAlex ID or URL, an arXiv id / URL / DOI, or the
    paper's title.
    """
    text = (paper or "").strip()
    if not text:
        return ("title", "")
    if "openalex.org" in text.lower():
        match = _OPENALEX_RE.search(text)
        if match:
            return ("openalex", match.group(1).upper())
    match = _ARXIV_REF_RE.search(text) or _ARXIV_BARE_RE.match(text)
    if match:
        return ("arxiv", match.group(1))
    match = _DOI_RE.search(text)
    if match:
        # Trailing punctuation is common when a DOI is pasted out of prose.
        doi = match.group(1).rstrip(").,;")
        arxiv = _ARXIV_DOI_RE.match(doi)
        return ("arxiv", arxiv.group(1)) if arxiv else ("doi", doi)
    match = _OPENALEX_RE.fullmatch(text.strip())
    if match:
        return ("openalex", match.group(1).upper())
    return ("title", text)


def _work_url(item: dict[str, Any]) -> str:
    """Landing page, else the DOI, else the OpenAlex record."""
    location = item.get("primary_location")
    if isinstance(location, dict):
        url = location.get("landing_page_url")
        if isinstance(url, str) and url.startswith("http"):
            return url
    doi = item.get("doi")
    if isinstance(doi, str) and doi:
        return doi if doi.startswith("http") else f"https://doi.org/{doi}"
    ident = item.get("id")
    return ident if isinstance(ident, str) else ""


def _short_id(value: Any) -> str:
    """`https://openalex.org/W123` -> `W123`."""
    if not isinstance(value, str):
        return ""
    return value.rsplit("/", 1)[-1]


def _bare_doi(value: Any) -> str:
    """OpenAlex stores DOIs as URLs; Crossref wants the bare form."""
    if not isinstance(value, str) or not value:
        return ""
    return value.split("doi.org/", 1)[-1].strip()


def _node(item: dict[str, Any]) -> dict[str, Any]:
    year = item.get("publication_year")
    cited = item.get("cited_by_count")
    return {
        "title": clip(item.get("display_name"), cap=300),
        "url": _work_url(item),
        "doi": _bare_doi(item.get("doi")),
        "openalex_id": _short_id(item.get("id")),
        "year": year if isinstance(year, int) else None,
        "cited_by_count": cited if isinstance(cited, int) else 0,
        "retracted": bool(item.get("is_retracted")),
    }


async def _arxiv_title(arxiv_id: str) -> str:
    """The title arXiv holds for `arxiv_id`, or "" — never raises."""
    try:
        feed = await _api._request_text(f"{_ARXIV_API}?id_list={quote(arxiv_id)}&max_results=1")
    except Exception:
        log.debug("arxiv title lookup failed for %s", arxiv_id, exc_info=True)
        return ""
    match = _ATOM_ENTRY_TITLE_RE.search(feed or "")
    return " ".join(html.unescape(match.group(1)).split()) if match else ""


async def _resolve_arxiv(arxiv_id: str) -> tuple[dict[str, Any] | None, list[str], str]:
    """`(work, near_misses, title)` for an arXiv preprint.

    OpenAlex does not resolve arXiv's DataCite DOIs: `works/https://doi.org/
    10.48550/arXiv.1706.03762` is a 404 for a paper it plainly indexes (under
    the DOI of the published version, or none). So `paper_graph` answered the
    most-cited preprint of the decade with "the DOI may be unregistered or
    mistyped". The way round is the one a person would take: ask arXiv what the
    paper is called, then find that title in OpenAlex.
    """
    title = await _arxiv_title(arxiv_id)
    if not title:
        return (None, [], "")
    candidates = await _title_candidates(title)
    # This title is arXiv's own, not something typed from memory, so an exact
    # match is meaningful. OpenAlex can hold more than one record under one
    # exact title (a preprint and its proceedings version); among those the
    # most-cited is the one the citation graph hangs off. With a single exact
    # match — the usual case — this is simply that record.
    #
    # The record's own metadata is OpenAlex's and is passed through as given:
    # on 2026-09-21 its entry for "Attention Is All You Need" (W2626778328)
    # carried publication_year 2025 and a non-arXiv DOI. That is upstream data,
    # not a wrong match.
    exact = [
        c for c in candidates
        if isinstance(c.get("display_name"), str)
        and _title_score(title, c["display_name"]) >= _EXACT_TITLE
    ]
    if exact:
        return (max(exact, key=lambda c: c.get("cited_by_count") or 0), [], title)
    work, near = _pick_by_title(title, candidates)
    return (work, near, title)


async def _resolve(
    kind: str, value: str
) -> tuple[dict[str, Any] | None, list[str]]:
    """`(work, near_misses)`. `work` is None when nothing matched confidently.

    The near misses are titles a TITLE lookup found and rejected. Naming
    them turns a dead end into a next step: the caller can see that the
    index has "BERT: Pre-training of…" under a slightly different title and
    retry, instead of concluding the paper is not indexed.
    """
    if kind == "doi":
        url = (
            f"{_WORKS}/https://doi.org/{quote(value, safe='/')}"
            f"?select={_SELECT_TARGET}{_mailto()}"
        )
        payload = await _api._get_json(url)
        found = payload if isinstance(payload, dict) and payload.get("id") else None
        return (found, [])
    if kind == "openalex":
        url = f"{_WORKS}/{quote(value, safe='')}?select={_SELECT_TARGET}{_mailto()}"
        payload = await _api._get_json(url)
        found = payload if isinstance(payload, dict) and payload.get("id") else None
        return (found, [])
    if not value:
        return (None, [])
    return _pick_by_title(value, await _title_candidates(value))


async def _title_candidates(title: str) -> list[dict[str, Any]]:
    """OpenAlex's first page of works for `title`, in its relevance order."""
    url = (
        f"{_WORKS}?search={quote_plus(title)}&per-page={_TITLE_CANDIDATES}"
        f"&select={_SELECT_TARGET}{_mailto()}"
    )
    payload = await _api._get_json(url)
    items = payload.get("results") if isinstance(payload, dict) else None
    return [i for i in items if isinstance(i, dict)] if isinstance(items, list) else []


def _pick_by_title(
    title: str, candidates: list[dict[str, Any]]
) -> tuple[dict[str, Any] | None, list[str]]:
    match = _best_title_match(title, candidates)
    if match is not None:
        return (match, [])
    near = [
        c["display_name"]
        for c in candidates
        if isinstance(c.get("display_name"), str) and c["display_name"].strip()
    ]
    return (None, near[:3])


def _title_score(query: str, title: str) -> float:
    """How faithfully `query` renders `title`, 0-100.

    Two measures, because neither alone works:

      * whole-string `token_sort_ratio` recognises an exact title but punishes
        a caller who typed only the first half — the real
        "Reciprocal rank fusion outperforms condorcet and individual rank
        learning methods" scores 70 against its own opening words;
      * the same ratio against the title TRUNCATED to the query's length
        recognises that prefix at 100, and is what separates a prefix from a
        title that merely contains the words further in: "attention is all you
        need" scores 100 against "Attention Is All You Need" and 75 against
        "Channel Attention Is All You Need for Video Frame Interpolation",
        where an intersection-based score calls both perfect.

    `token_set_ratio` is deliberately not used: it scores the intersection, so
    every superset title ties at 100.
    """
    normalized_query = utils.default_process(query)
    normalized_title = utils.default_process(title)
    whole = fuzz.token_sort_ratio(normalized_query, normalized_title)
    words = normalized_title.split()[: len(normalized_query.split())]
    prefix = fuzz.token_sort_ratio(normalized_query, " ".join(words))
    return float(max(whole, prefix))


def _best_title_match(
    query: str, items: list[dict[str, Any]]
) -> dict[str, Any] | None:
    """The first candidate that is confidently the paper, or None.

    FIRST, not highest-scoring: `items` arrives in OpenAlex relevance order,
    which carries information no string metric has. All three of "Attention Is
    All You Need", "…In Speech Separation" and "Channel Attention…" answer to
    the same typed title, and relevance is what knows which one the caller
    meant.

    The score is a GATE, not the ranking. It exists because relevance order
    alone is not enough either: OpenAlex answers "BERT pre-training of deep
    bidirectional transformers" with BioBERT, Sentence-BERT, T5, BEiT and
    AlphaFold — the actual BERT paper is not in the page at all — and taking
    the top hit returned a different paper's citation graph under the heading
    the caller typed.
    """
    for item in items:
        title = item.get("display_name")
        if not isinstance(title, str) or not title.strip():
            continue
        if _title_score(query, title) >= _TITLE_MIN:
            return item
    return None


async def _references(work: dict[str, Any], limit: int) -> tuple[list[dict[str, Any]], int]:
    """Restore `referenced_works` to full records. Returns (nodes, requested).

    One batched call, not one per reference. The returned order is OpenAlex's,
    which is not meaningful, so the caller sorts.
    """
    ids = [
        _short_id(w)
        for w in (work.get("referenced_works") or [])
        if isinstance(w, str)
    ]
    ids = [i for i in ids if i]
    if not ids:
        return ([], 0)
    wanted = ids[:_MAX_BATCH]
    url = (
        f"{_WORKS}?filter=openalex_id:{'|'.join(wanted)}"
        f"&per-page={len(wanted)}&select={_SELECT}{_mailto()}"
    )
    payload = await _api._get_json(url)
    items = payload.get("results") if isinstance(payload, dict) else None
    nodes = [_node(i) for i in items if isinstance(i, dict)] if isinstance(items, list) else []
    nodes.sort(key=lambda n: -(n["cited_by_count"] or 0))
    return (nodes[:limit], len(ids))


async def _citations(work_id: str, limit: int) -> list[dict[str, Any]]:
    """Works citing this one, most-cited first.

    Sorting by citation count rather than date is the point: the goal is "what
    became of this result", and the answer is whichever follow-up the field
    itself converged on, not whatever was posted most recently.
    """
    if not work_id:
        return []
    url = (
        f"{_WORKS}?filter=cites:{quote(work_id, safe='')}"
        f"&sort=cited_by_count:desc&per-page={limit}&select={_SELECT}{_mailto()}"
    )
    payload = await _api._get_json(url)
    items = payload.get("results") if isinstance(payload, dict) else None
    if not isinstance(items, list):
        return []
    return [_node(i) for i in items if isinstance(i, dict)]


async def _crossref_status(doi: str) -> dict[str, Any]:
    """Registration and post-publication notices for `doi`.

    `registered=False` is a real signal, not a failure: a DOI a model invented
    resolves nowhere, and that is exactly what a citation check needs to catch.
    """
    status: dict[str, Any] = {"registered": None, "notices": []}
    if not doi:
        return status
    url = f"{_CROSSREF}/{quote(doi, safe='/')}"
    if settings.contact_email:
        url = f"{url}?mailto={quote_plus(settings.contact_email)}"
    # `_get_json` maps a 404 and a timeout to the same `None`, so without
    # reading the status back, "unknown DOI" and "Crossref was slow" would be
    # indistinguishable — and reporting the second as the first is exactly the
    # kind of confident wrong answer this tool exists to prevent.
    token = _last_http_status.set(None)
    try:
        payload = await _api._get_json(url)
        http_status = _last_http_status.get()
    finally:
        _last_http_status.reset(token)
    if not isinstance(payload, dict):
        if http_status == 404:
            status["registered"] = False
        # Any other failure leaves `registered` as None, so the renderer says
        # nothing rather than something false.
        return status
    message = payload.get("message")
    if not isinstance(message, dict):
        return status
    status["registered"] = True
    best: dict[str, dict[str, Any]] = {}
    for entry in message.get("updated-by") or []:
        if not isinstance(entry, dict):
            continue
        kind = entry.get("type")
        notice_doi = entry.get("DOI")
        if not isinstance(kind, str) or not isinstance(notice_doi, str):
            continue
        label = entry.get("label")
        notice = {
            "type": kind,
            "doi": notice_doi,
            "date": _crossref_date(entry.get("updated")),
            "label": label if isinstance(label, str) else "",
        }
        current = best.get(notice_doi)
        if current is None or _notice_rank(kind) < _notice_rank(current["type"]):
            best[notice_doi] = notice
    notices = sorted(
        best.values(), key=lambda n: (_notice_rank(n["type"]), n["date"] or "")
    )
    status["notices"] = notices[:_MAX_NOTICES]
    return status


def _notice_rank(kind: str) -> int:
    return _NOTICE_RANK.get(kind.lower(), len(_NOTICE_RANK))


def _crossref_date(updated: Any) -> str:
    """`{"date-parts": [[2020, 5, 22]]}` -> `2020-05-22`, truncating at a gap."""
    if not isinstance(updated, dict):
        return ""
    parts = updated.get("date-parts")
    if not isinstance(parts, list) or not parts or not isinstance(parts[0], list):
        return ""
    numbers: list[int] = []
    for part in parts[0]:
        if not isinstance(part, int):
            break
        numbers.append(part)
    if not numbers:
        return ""
    return "-".join(
        [str(numbers[0])] + [f"{n:02d}" for n in numbers[1:3]]
    )


async def paper_graph(
    paper: str,
    direction: Direction = "both",
    limit: int = 10,
) -> dict[str, Any]:
    """Resolve `paper` and walk its citation graph. Never raises."""
    limit = max(1, min(int(limit or 10), _MAX_LIMIT))
    if direction not in ("both", "references", "citations"):
        direction = "both"

    kind, value = _normalize(paper)
    arxiv_title = ""
    if kind == "arxiv":
        work, near_misses, arxiv_title = await _resolve_arxiv(value)
    else:
        work, near_misses = await _resolve(kind, value)
    if not work:
        if kind == "arxiv":
            reason = (
                f" arXiv calls it {arxiv_title!r}, but OpenAlex has no confident match for "
                "that title. A very recent preprint may not be indexed yet."
                if arxiv_title
                else f" arXiv returned no record for {value!r}; check the identifier."
            )
        elif kind == "doi":
            reason = " The DOI may be unregistered or mistyped."
        else:
            reason = " Try the exact title, or a DOI."
        notes = [f"No work matched {paper!r} in OpenAlex." + reason]
        if near_misses:
            listed = "; ".join(f"{t!r}" for t in near_misses)
            notes.append(
                "Closest titles found, none close enough to act on: " + listed
            )
        return {
            "query": paper,
            "resolved_as": kind,
            "paper": None,
            "direction": direction,
            "references": [],
            "citations": [],
            "notes": notes,
        }

    target = _node(work)
    notes: list[str] = []
    if kind == "arxiv":
        # Say how it was found: a title match is a weaker identification than a
        # DOI, and the caller should be able to see which one they got.
        notes.append(
            f"Resolved arXiv:{value} by its title ({arxiv_title!r}): OpenAlex does not "
            "resolve arXiv's own DOIs. Check the paper above is the one you meant."
        )

    references: list[dict[str, Any]] = []
    total_references = 0
    if direction in ("both", "references"):
        references, total_references = await _references(work, limit)
        if total_references and not references:
            # OpenAlex lists edges it cannot always restore — merged or
            # withdrawn records. Silence here would read as "no references".
            notes.append(
                f"{total_references} references are recorded but none could be "
                "restored; the target's own record may be incomplete."
            )
        elif total_references > len(references):
            notes.append(
                f"Showing {len(references)} of {total_references} references, "
                "most-cited first."
            )

    citations: list[dict[str, Any]] = []
    if direction in ("both", "citations"):
        citations = await _citations(target["openalex_id"], limit)
        if target["cited_by_count"] > len(citations):
            notes.append(
                f"Showing {len(citations)} of {target['cited_by_count']} citing "
                "works, most-cited first."
            )

    status = await _crossref_status(target["doi"])
    retractions = [n for n in status["notices"] if n["type"] == "retraction"]
    if target["retracted"] or retractions:
        target["retracted"] = True
        notes.insert(0, "RETRACTED. Do not cite this paper as a standing result.")
    elif status["registered"] is False:
        # NOT the same as "fabricated": Crossref registers journal DOIs, and
        # DataCite registers most dataset and repository DOIs (Zenodo,
        # figshare). The work resolved in OpenAlex, so the DOI is real — only
        # its post-publication notices are unavailable here.
        notes.insert(
            0,
            "Crossref has no record of this DOI. It is likely a DataCite DOI "
            "(dataset or repository), so retraction notices could not be checked.",
        )

    target["crossref"] = status
    return {
        "query": paper,
        "resolved_as": kind,
        "paper": target,
        "direction": direction,
        "references": references,
        "citations": citations,
        "notes": notes,
    }
