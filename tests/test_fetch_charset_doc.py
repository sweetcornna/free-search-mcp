"""Offline tests for the fetcher charset decode + binary-document routing.

The helpers under test are pure/sync (no event loop, no network): charset
detection from header/meta, and the URL/content-type document classifiers
that route PDFs/DOCX to the document parser instead of decoding bytes as text.
"""
from __future__ import annotations

from search_mcp.fetcher import (
    _ctype_is_markup,
    _decode_body,
    _is_document_ctype,
    _is_document_url,
)


# --- charset decode --------------------------------------------------------


def test_decode_honors_header_charset_gbk():
    # GBK-encoded Chinese must not be decoded as UTF-8 (would be mojibake).
    text = "中文内容"
    body = text.encode("gbk")
    out = _decode_body(body, "text/html; charset=gbk")
    assert out == text


def test_decode_sniffs_meta_charset_when_header_lacks_one():
    text = "日本語のテスト"
    body = (
        b"<html><head><meta charset='shift_jis'></head><body>"
        + text.encode("shift_jis")
        + b"</body></html>"
    )
    out = _decode_body(body, "text/html")  # header has no charset
    assert text in out


def test_decode_defaults_to_utf8_and_never_raises_on_bad_codec():
    text = "ünïcode ✓ 中文"
    body = text.encode("utf-8")
    # No charset anywhere -> UTF-8.
    assert _decode_body(body, "text/html") == text
    # Bogus codec label -> falls back to UTF-8 instead of raising LookupError.
    assert _decode_body(body, "text/html; charset=not-a-real-codec") == text


# --- document routing classifiers -----------------------------------------


def test_is_document_url_matches_pdf_and_docx_suffixes():
    assert _is_document_url("https://arxiv.org/pdf/1706.03762.pdf")
    assert _is_document_url("https://example.com/report.docx")
    assert _is_document_url("https://example.com/paper.PDF?download=1".split("?")[0])
    assert not _is_document_url("https://example.com/article.html")
    assert not _is_document_url("https://example.com/")


def test_is_document_ctype_matches_binary_doc_types():
    assert _is_document_ctype("application/pdf")
    assert _is_document_ctype(
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    )
    assert not _is_document_ctype("text/html; charset=utf-8")
    assert not _is_document_ctype("application/json")


def test_ctype_is_markup_treats_empty_as_recoverable():
    assert _ctype_is_markup("")  # http fetch failed -> allow browser fallback
    assert _ctype_is_markup("text/html")
    assert _ctype_is_markup("application/xml")
    assert not _ctype_is_markup("application/json")
    assert not _ctype_is_markup("text/plain")
