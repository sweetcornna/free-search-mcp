"""Offline tests for the Google News URL resolver (gnews.py).

The network resolve is not exercised here (it needs live google news); these
cover the pure parts: URL classification, the batchexecute request payload,
and the response parser that pulls the publisher URL out of Google's RPC reply.
"""
from __future__ import annotations

import json

import pytest

from search_mcp.gnews import (
    _build_freq,
    _parse_batchexecute,
    is_google_news_url,
    resolve_google_news_url,
)


def test_is_google_news_url_matches_article_blobs():
    assert is_google_news_url("https://news.google.com/rss/articles/CBMiXXXX")
    assert is_google_news_url("https://news.google.com/articles/CBMiYYYY?oc=5")
    assert is_google_news_url("https://news.google.com/read/CBMiZZZZ")


def test_is_google_news_url_rejects_non_news_and_non_article():
    assert not is_google_news_url("https://www.reuters.com/world/x")
    assert not is_google_news_url("https://news.google.com/topics/abc")
    assert not is_google_news_url("https://example.com/rss/articles/CBM")
    assert not is_google_news_url("not-a-url")


def test_build_freq_encodes_id_ts_sig():
    freq = _build_freq("ARTICLE_ID", "1700000000", "SIGNATURE")
    assert freq.startswith("f.req=")
    # The inner payload (url-decoded) must carry our id/ts/sig and the RPC name.
    from urllib.parse import unquote

    decoded = unquote(freq[len("f.req=") :])
    assert "Fbv4je" in decoded
    assert "garturlreq" in decoded
    assert "ARTICLE_ID" in decoded
    assert "SIGNATURE" in decoded
    assert "1700000000" in decoded


def test_parse_batchexecute_extracts_publisher_url():
    inner = json.dumps(["garturlres", "https://www.reuters.com/world/story-2026", 1])
    body = (
        ")]}'\n\n"
        + json.dumps(
            [["wrb.fr", "Fbv4je", inner, None, None, None, "generic"], ["di", 13]]
        )
    )
    assert _parse_batchexecute(body) == "https://www.reuters.com/world/story-2026"


def test_parse_batchexecute_returns_none_on_garbage():
    assert _parse_batchexecute("") is None
    assert _parse_batchexecute(")]}'\n\nnot json at all") is None
    # Right shape but no Fbv4je row -> None.
    assert _parse_batchexecute(")]}'\n" + json.dumps([["di", 13]])) is None


def test_parse_batchexecute_handles_multiline_and_length_prefix():
    # A pretty-printed / length-prefixed (chunked) response defeats the single-
    # line structured scan; the regex fallback still recovers the publisher url.
    body = (
        ")]}'\n\n"
        "359\n"  # chunk length prefix line
        "[[\"wrb.fr\",\"Fbv4je\",\n"
        "  \"[\\\"garturlres\\\",\\\"https://www.bbc.com/news/articles/abc123\\\",1]\",\n"
        "  null,null,null,\"generic\"]]\n"
    )
    assert (
        _parse_batchexecute(body)
        == "https://www.bbc.com/news/articles/abc123"
    )


@pytest.mark.asyncio
async def test_resolve_returns_none_for_non_gnews_url():
    # A non-Google-News URL short-circuits without any network call.
    assert await resolve_google_news_url("https://www.bbc.com/news/x") is None
