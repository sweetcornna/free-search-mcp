"""Does a bucket of results answer the query, or only its first word?

A blocked engine is easy to see: a captcha page, a 403, zero results. The
failure this module exists for is the one that looks like success. Bing, when
it dislikes a request, answers HTTP 200 with ten well-formed organic results
that match only the FIRST token of the query — "rust ownership borrowing"
returns the Steam page for the game Rust, "postgres explain analyze" returns
the postgresql.org home page and its Wikipedia article. Every structural check
passes. Reciprocal-rank fusion then interleaves those results at ranks 3, 6, 9
of every answer, which is what "the search is inaccurate" turned out to mean.

The signal is whole-bucket, and deliberately so. A per-result lexical bonus was
measured and rejected (evals/ranking/README.md: MRR 0.747 -> 0.645) because a
good result often does not echo the query. A whole BUCKET that never mentions
anything past the first word is a different kind of evidence: measured on live
traffic (2026-09), healthy buckets score 0.5-1.0 and decoy buckets 0.0-0.2,
with nothing in between.

Imported by both the aggregator and the Bing engine, so it must not import
either of them.
"""

from __future__ import annotations

import re
from collections.abc import Iterable
from typing import Any

# Share of a bucket's results that must mention some query term beyond the
# first. Measured 2026-09-21 on fresh queries: duckduckgo / anysearch 1.00,
# brave >= 0.5, so360 >= 0.6; Bing decoys 0.00-0.20; a bad public searx
# instance 0.0-0.2. 0.3 sits in the empty gap between the two populations.
COHERENCE_MIN = 0.3

# A bucket this coherent proves the query CAN be echoed, which is what makes
# another bucket's silence meaningful. See `aggregator._drop_decoy_buckets`.
WITNESS_MIN = 0.5

# Below these the ratio is noise, and an unevaluable bucket is always kept.
_MIN_RESULTS = 4
_MIN_REST_TERMS = 2


def _is_cjk(c: str) -> bool:
    o = ord(c)
    return (
        0x4E00 <= o <= 0x9FFF       # CJK unified ideographs
        or 0x3040 <= o <= 0x30FF    # Japanese hiragana/katakana
        or 0xAC00 <= o <= 0xD7A3    # Korean hangul syllables
    )


def _lead_query_terms(query: str) -> set[str]:
    """Tokenize a query for snippet-substring matching.

    Pure-ASCII tokens: keep when len > 3 (skip "the", "vs", "of"...).
    CJK tokens: extract char-bigrams ("模型架构" -> {"模型","型架","架构"})
    so we still match when the snippet splits the term into "模型" and
    "架构" separately rather than emitting the whole 4-char run.
    Mixed-script tokens are included as-is when they contain a length-3+ ASCII
    portion or any CJK at all.
    """
    terms: set[str] = set()
    for tok in query.split():
        cjk_chars = [c for c in tok if _is_cjk(c)]
        if len(cjk_chars) >= 2:
            for i in range(len(cjk_chars) - 1):
                terms.add(cjk_chars[i] + cjk_chars[i + 1])
        elif len(cjk_chars) == 1:
            # Single CJK char alone is too generic; skip.
            pass
        elif len(tok) > 3:
            terms.add(tok.lower())
    return terms


# ---------------------------------------------------------------------------
# Coherence terms
# ---------------------------------------------------------------------------
#
# Not `_lead_query_terms`: that one drops every token of three letters or
# fewer, and "vietnam gdp growth" needs its "gdp". This tokenizer keeps short
# tokens but matches them on word boundaries, drops stopwords (a decoy page
# contains "the" and "how" like any other page), and strips search operators,
# which no result is expected to echo.

_OPERATOR_RE = re.compile(
    r"^[+-]?(?:site|filetype|ext|intitle|allintitle|inurl|allinurl|intext|inbody"
    r"|inanchor|lang|language|loc|location|before|after|feed|contains|ip|prefer):",
    re.IGNORECASE,
)
_BOOLEAN_TOKENS = frozenset({"OR", "AND", "NOT", "|", "&"})

_STOPWORDS = frozenset(
    {
        "a", "about", "an", "and", "are", "as", "at", "be", "been", "best", "by",
        "can", "could", "did", "do", "does", "for", "from", "get", "had", "has",
        "have", "how", "i", "if", "in", "into", "is", "it", "its", "me", "my",
        "new", "no", "not", "of", "on", "or", "our", "should", "so", "than", "that",
        "the", "their", "then", "there", "these", "they", "this", "to", "top", "up",
        "us", "use", "using", "vs", "was", "we", "were", "what", "when", "where",
        "which", "who", "why", "will", "with", "would", "you", "your",
    }
)

# Built from code points, not typed as a character class: the same three
# ranges as `_is_cjk`, and a literal CJK range is unreadable in a diff.
_CJK_RANGES = ((0x4E00, 0x9FFF), (0x3040, 0x30FF), (0xAC00, 0xD7A3))
_CJK_RUN_RE = re.compile(
    "[" + "".join(f"{chr(lo)}-{chr(hi)}" for lo, hi in _CJK_RANGES) + "]+"
)
# A word, optionally carrying a version tail: "3.12" stays one term, while
# "asyncio.TaskGroup" splits into the two words a page will actually print.
_WORD_RE = re.compile(r"[^\W_]+(?:\.\d+)+|[^\W_]+")
_SUFFIXES = ("ing", "ed", "es", "s")


def _stem(word: str) -> str:
    """The crudest stem that lets "borrowing" find "borrow checker".

    ASCII only, and never below four letters, so it cannot turn a real word
    into a fragment that matches everything."""
    if not word.isascii() or not word.isalpha() or word.endswith("ss"):
        return word
    for suffix in _SUFFIXES:
        if word.endswith(suffix) and len(word) - len(suffix) >= 4:
            return word[: -len(suffix)]
    return word


def _bigrams(run: str, start: int = 0) -> list[str]:
    return [run[i : i + 2] for i in range(start, len(run) - 1)]


def _token_terms(token: str) -> list[str]:
    """The matchable terms of one whitespace-delimited query token, in order."""
    terms: list[str] = []
    for run in _CJK_RUN_RE.findall(token):
        terms.extend(_bigrams(run))
    latin = _CJK_RUN_RE.sub(" ", token).lower()
    for word in _WORD_RE.findall(latin):
        if len(word) < 2 or word in _STOPWORDS:
            continue
        terms.append(_stem(word))
    return terms


def _content_tokens(query: str) -> list[str]:
    """Query tokens that a relevant result could be expected to echo."""
    tokens: list[str] = []
    in_excluded_phrase = False
    for tok in query.split():
        if in_excluded_phrase:
            # The tail of `-"some phrase"`: still part of the exclusion.
            in_excluded_phrase = not tok.endswith('"')
            continue
        if tok.startswith('-"'):
            in_excluded_phrase = not (len(tok) > 2 and tok.endswith('"'))
            continue
        if tok in _BOOLEAN_TOKENS or tok.startswith("-") or _OPERATOR_RE.match(tok):
            continue
        tokens.append(tok)
    return tokens


def coherence_terms(query: str) -> tuple[list[str], list[str]]:
    """Split a query into (anchor, rest).

    `anchor` is what a decoy still matches: the terms of the first content
    token. `rest` is everything after it — the part a decoy ignores — minus
    anything that overlaps the anchor ("postgresql" proves nothing once
    "postgres" is the anchor).

    An unsegmented CJK query is one token, so it is split inside the run: the
    first bigram anchors, and `rest` starts at the third character. The bigram
    straddling the two ("上海明珠" -> "海明") belongs to neither.
    """
    tokens = _content_tokens(query)
    anchor: list[str] = []
    rest: list[str] = []

    if len(tokens) == 1:
        runs = _CJK_RUN_RE.findall(tokens[0])
        if len(runs) == 1 and runs[0] == tokens[0].strip("\"'“”《》「」") and len(runs[0]) >= 4:
            run = runs[0]
            anchor = [run[:2]]
            rest = _bigrams(run, start=2)
    if not anchor:
        for i, tok in enumerate(tokens):
            terms = _token_terms(tok)
            if not terms:
                continue
            anchor = terms
            for later in tokens[i + 1 :]:
                rest.extend(_token_terms(later))
            break

    seen: set[str] = set()
    unique_rest: list[str] = []
    for term in rest:
        if term in seen or any(term in a or a in term for a in anchor):
            continue
        seen.add(term)
        unique_rest.append(term)
    return anchor, unique_rest


def _matcher(terms: Iterable[str]) -> re.Pattern[str]:
    parts: list[str] = []
    for term in terms:
        escaped = re.escape(term)
        # Two- and three-letter terms only count as whole words: "gdp" must not
        # be found inside nothing, but "go" is inside "google" and "ai" inside
        # "again", and a decoy that matches by accident is a decoy that passes.
        if len(term) <= 3 and not _CJK_RUN_RE.fullmatch(term):
            parts.append(rf"(?<!\w){escaped}(?!\w)")
        else:
            parts.append(escaped)
    return re.compile("|".join(parts))


def _field(result: Any, name: str) -> str:
    value = result.get(name) if isinstance(result, dict) else getattr(result, name, "")
    return value or ""


def bucket_coherence(query: str, results: Iterable[Any]) -> float | None:
    """Share of `results` that mention any query term beyond the first token.

    None means "cannot tell" — a one- or two-word query, or too few results —
    and callers must treat None as healthy. Accepts `SearchResult` objects and
    result dicts alike. The URL counts as evidence: documentation pages often
    carry the term in the path and nowhere in a truncated snippet.
    """
    rows = list(results)
    if len(rows) < _MIN_RESULTS:
        return None
    anchor, rest = coherence_terms(query)
    if not anchor or len(rest) < _MIN_REST_TERMS:
        return None
    matcher = _matcher(rest)
    hits = 0
    for row in rows:
        haystack = " ".join(_field(row, f) for f in ("title", "snippet", "url")).lower()
        if matcher.search(haystack):
            hits += 1
    return hits / len(rows)


def looks_like_decoy(query: str, results: Iterable[Any]) -> bool:
    """True when the bucket is evaluable and almost none of it is on topic."""
    score = bucket_coherence(query, results)
    return score is not None and score < COHERENCE_MIN


def is_witness(query: str, results: Iterable[Any]) -> bool:
    """True when the bucket proves this query's terms do get echoed."""
    score = bucket_coherence(query, results)
    return score is not None and score >= WITNESS_MIN
