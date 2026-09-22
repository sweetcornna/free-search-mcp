"""The decoy signal: a bucket that only ever matches the query's first word.

The titles below are real. They are what Bing returned, with HTTP 200 and
flawless markup, in the ranking capture committed on 2026-08-29 — three weeks
before anyone noticed, because every structural assertion about that response
holds.
"""
from __future__ import annotations

from search_mcp.coherence import (
    COHERENCE_MIN,
    bucket_coherence,
    coherence_terms,
    is_witness,
    looks_like_decoy,
)
from search_mcp.engines.base import SearchResult


def rows(*pairs: tuple[str, str]) -> list[dict]:
    return [
        {"title": t, "snippet": s, "url": f"https://example.org/{i}"}
        for i, (t, s) in enumerate(pairs)
    ]


RUST_DECOY = rows(
    ("Rust on Steam", "The only aim in Rust is to survive. Everything wants you to die."),
    ("Rust — Explore, Build and Survive", "The only aim in Rust is to survive."),
    ("Rust Programming Language", "Rust in production. Hundreds of companies around the world"),
    ("Install Rust - Rust Programming Language", "A language empowering everyone to build"),
    ("Rust (programming language) - Wikipedia", "Rust is a general-purpose programming language"),
    ("Rust (video game) - Wikipedia", "Rust is a 2018 multiplayer survival video game"),
    ("Rust+ - Official Companion App — Rust", "The official Rust companion app."),
    ("Rust - Download", "May 7, 2026 · Rust is a PvP-driven game with deep crafting"),
)

RUST_HEALTHY = rows(
    ("Understanding Ownership - The Rust Programming Language", "Ownership is Rust's most"),
    ("References and Borrowing - The Rust Programming Language", "The issue with the tuple"),
    ("Rust Basics Explained: Borrowing in Rust", "Borrowing is Rust's way of letting you use"),
    ("Improved Rust Borrow Checker Enters Testing", "The new borrow checker, Polonius"),
    ("Ownership & Borrowing · Learn Rust", "Ownership is Rust's most unique feature"),
)

VIETNAM_DECOY = rows(
    ("Vietnam - Wikipedia", "Upon the North Vietnamese victory in 1975, Vietnam reunified"),
    ("Vietnam | History, Population, Map, Flag, Government, & Facts", "Vietnam, country"),
    ("Visit Vietnam: The Official Tourism Website of Vietnam", "Discover the highlights"),
    ("Vietnam Maps & Facts - World Atlas", "Vietnam is situated in Southeast Asia"),
    ("History of Vietnam - Wikipedia", "The Second World War brought a 5-year occupation"),
)


def test_the_captured_decoys_are_recognised():
    assert bucket_coherence("rust ownership borrowing", RUST_DECOY) == 0.0
    assert looks_like_decoy("rust ownership borrowing", RUST_DECOY)
    assert looks_like_decoy("vietnam gdp growth", VIETNAM_DECOY)


def test_a_healthy_bucket_for_the_same_query_is_a_witness():
    score = bucket_coherence("rust ownership borrowing", RUST_HEALTHY)
    assert score == 1.0
    assert not looks_like_decoy("rust ownership borrowing", RUST_HEALTHY)
    assert is_witness("rust ownership borrowing", RUST_HEALTHY)


def test_the_cut_sits_between_the_two_measured_populations():
    # Healthy buckets measured 0.5-1.0 and decoys 0.0-0.2. If someone moves the
    # constant they should have new measurements, not a hunch.
    assert 0.2 < COHERENCE_MIN < 0.5


def test_a_mostly_off_topic_bucket_with_one_stray_hit_is_still_a_decoy():
    bucket = RUST_DECOY[:7] + rows(("Rust ownership explained", "borrowing and lifetimes"))
    score = bucket_coherence("rust ownership borrowing", bucket)
    assert score == 1 / 8
    assert looks_like_decoy("rust ownership borrowing", bucket)


def test_short_terms_survive_and_match_whole_words_only():
    """`_lead_query_terms` drops "gdp"; this tokenizer must not, or "vietnam gdp
    growth" is a one-term query and the decoy walks through."""
    assert coherence_terms("vietnam gdp growth") == (["vietnam"], ["gdp", "growth"])
    # ...but a three-letter term inside a longer word is not a mention of it.
    bucket = rows(*[("Vietnam travel", "the gdpr notice on a tourism page")] * 4)
    assert bucket_coherence("vietnam gdp growth", bucket) == 0.0


def test_a_light_stem_lets_borrowing_find_borrow():
    assert coherence_terms("rust ownership borrowing")[1] == ["ownership", "borrow"]
    # Never a fragment: "class" is not a plural, and "runs" would leave "run".
    assert coherence_terms("class hierarchy")[0] == ["class"]
    assert coherence_terms("runs slowly")[0] == ["runs"]


def test_one_and_two_word_queries_cannot_be_evaluated():
    assert bucket_coherence("rust", RUST_DECOY) is None
    assert bucket_coherence("rust ownership", RUST_DECOY) is None
    assert not looks_like_decoy("rust ownership", RUST_DECOY)
    assert not is_witness("rust ownership", RUST_HEALTHY)


def test_too_few_results_cannot_be_evaluated():
    assert bucket_coherence("rust ownership borrowing", RUST_DECOY[:3]) is None
    assert bucket_coherence("rust ownership borrowing", RUST_DECOY[:4]) == 0.0


def test_operators_and_exclusions_are_not_expected_to_be_echoed():
    anchor, rest = coherence_terms(
        'rust site:doc.rust-lang.org ownership -game -"video game" filetype:pdf OR borrowing'
    )
    assert anchor == ["rust"]
    assert rest == ["ownership", "borrow"]


def test_stopwords_prove_nothing():
    # Every page contains "how", "to" and "the", decoys included.
    assert coherence_terms("python how to read the file") == (["python"], ["read", "file"])


def test_a_rest_term_that_overlaps_the_anchor_proves_nothing():
    # A page about PostgreSQL mentions "postgresql". That is the anchor again.
    anchor, rest = coherence_terms("postgres postgresql explain")
    assert rest == ["explain"]
    assert bucket_coherence("postgres postgresql explain", RUST_DECOY) is None


def test_segmented_chinese_uses_bigrams():
    anchor, rest = coherence_terms("上海 天气 预报")
    assert anchor == ["上海"]
    assert rest == ["天气", "预报"]
    decoy = rows(*[("上海 - 维基百科", "上海市是中华人民共和国的直辖市")] * 5)
    healthy = rows(*[("上海天气预报", "上海今天多云，未来一周天气")] * 5)
    assert looks_like_decoy("上海 天气 预报", decoy)
    assert is_witness("上海 天气 预报", healthy)


def test_unsegmented_chinese_splits_inside_the_run():
    anchor, rest = coherence_terms("上海明珠塔门票价格")
    assert anchor == ["上海"]
    # "海明" straddles the anchor and is neither; rest starts at the third char.
    assert "海明" not in rest
    assert rest == ["明珠", "珠塔", "塔门", "门票", "票价", "价格"]
    # Four characters leave a single rest bigram: not enough to judge.
    assert bucket_coherence("人工智能", rows(*[("人工", "人工")] * 5)) is None


def test_mixed_script_tokens_contribute_both_scripts():
    anchor, rest = coherence_terms("RoboMaster 2026赛季 规则手册")
    assert anchor == ["robomaster"]
    assert rest == ["赛季", "2026", "规则", "则手", "手册"]


def test_version_numbers_stay_whole_and_dotted_names_split():
    assert coherence_terms("python 3.12 asyncio.TaskGroup")[1] == ["3.12", "asyncio", "taskgroup"]


def test_the_url_counts_as_evidence():
    bucket = [
        {"title": "Documentation", "snippet": "", "url": f"https://pg.example/docs/explain-{i}"}
        for i in range(4)
    ]
    assert bucket_coherence("postgres explain analyze", bucket) == 1.0


def test_search_result_objects_are_accepted():
    bucket = [
        SearchResult(title=r["title"], url=r["url"], snippet=r["snippet"], engine="bing", rank=i)
        for i, r in enumerate(RUST_DECOY, 1)
    ]
    assert looks_like_decoy("rust ownership borrowing", bucket)


def test_the_helpers_are_still_importable_from_the_aggregator():
    # evals/ranking/replay.py and tests/test_dedup_and_lead.py import them there.
    from search_mcp import aggregator, coherence

    assert aggregator._lead_query_terms is coherence._lead_query_terms
    assert aggregator._is_cjk is coherence._is_cjk
