"""Binary/media resources through `fetch`.

The rule this file protects: a non-text resource is *described*, not decoded.
Decoding a PNG as text yields a screenful of U+FFFD, and inlining it by
default would spend four figures of tokens the caller never asked for.
"""

from __future__ import annotations

import zlib

import pytest

from search_mcp import fetcher
from search_mcp.fetcher import (
    _image_dimensions,
    _is_asset_ctype,
    _is_asset_url,
    fetch_page,
)

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.


def _png(width: int, height: int) -> bytes:
    """Minimal but genuinely valid PNG, so Pillow really parses the header."""

    def chunk(tag: bytes, data: bytes) -> bytes:
        body = tag + data
        return (
            len(data).to_bytes(4, "big")
            + body
            + zlib.crc32(body).to_bytes(4, "big")
        )

    ihdr = width.to_bytes(4, "big") + height.to_bytes(4, "big") + bytes([8, 2, 0, 0, 0])
    raw = b"".join(b"\x00" + b"\x00\x00\x00" * width for _ in range(height))
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw))
        + chunk(b"IEND", b"")
    )


@pytest.fixture
def served(monkeypatch):
    """Serve fixed bytes + content-type to BOTH fetch paths.

    `fetch_page` picks its route from the URL first and the content-type
    second, so an extension-less URL reaches the text downloader before
    anything realises it's an image. Stubbing only the asset downloader would
    send those tests to the real network and then to Chromium.
    """

    def _serve(blob: bytes, ctype: str):
        async def fake_stream(client, url, raise_for_status=True):
            return 200, ctype, blob

        async def fake_http_fetch(url):
            return ctype, blob.decode("utf-8", errors="replace")

        async def allow(url):
            return None

        monkeypatch.setattr(fetcher, "assert_url_allowed_async", allow)
        monkeypatch.setattr(fetcher, "_http_fetch", fake_http_fetch)
        monkeypatch.setattr("search_mcp.httpfetch.httpx_stream_capped", fake_stream)

    return _serve


# ---------------------------------------------------------------------------
# Classification
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "ctype,expected",
    [
        ("image/png", True), ("image/jpeg", True), ("video/mp4", True),
        ("audio/mpeg", True), ("font/woff2", True),
        ("application/octet-stream", True), ("application/zip", True),
        ("text/html", False), ("application/json", False),
        ("text/plain", False), ("", False),
        # SVG is markup — readable as text, so NOT an opaque asset.
        ("image/svg+xml", False),
    ],
)
def test_asset_content_types(ctype, expected):
    assert _is_asset_ctype(ctype) is expected


@pytest.mark.parametrize(
    "url,expected",
    [
        ("https://x.example/a.png", True),
        ("https://x.example/a.JPG", True),
        ("https://x.example/a.webp", True),
        ("https://x.example/a.svg", False),
        ("https://x.example/a.html", False),
        ("https://x.example/page", False),
    ],
)
def test_asset_urls(url, expected):
    assert _is_asset_url(url) is expected


# ---------------------------------------------------------------------------
# Dimensions
# ---------------------------------------------------------------------------


def test_dimensions_read_from_the_header():
    assert _image_dimensions(_png(12, 7)) == (12, 7)


def test_dimensions_of_a_non_image_are_unknown_not_an_error():
    """Dimensions are a nicety; failing to read them must not fail the fetch."""
    assert _image_dimensions(b"not an image") == (None, None)


# ---------------------------------------------------------------------------
# fetch behavior
# ---------------------------------------------------------------------------


async def test_image_is_described_not_decoded(served):
    served(_png(580, 164), "image/png")
    result = await fetch_page("https://x.example/logo.png", force_refresh=True)

    assert result.method == "asset"
    assert result.media_type == "image/png"
    assert (result.width, result.height) == (580, 164)
    assert result.bytes_size > 0
    assert len(result.sha256) == 64
    # The description, not the bytes.
    assert "logo.png" in result.content
    assert "580×164px" in result.content
    assert "�" not in result.content


async def test_bytes_are_withheld_by_default(served):
    """A plain fetch of a 1MB image must not cost a megabyte of base64."""
    served(_png(4, 4), "image/png")
    result = await fetch_page("https://x.example/a.png", force_refresh=True)
    assert result.data is None
    assert "inline=True" in result.content


async def test_inline_attaches_the_bytes(served):
    blob = _png(4, 4)
    served(blob, "image/png")
    result = await fetch_page("https://x.example/a.png", force_refresh=True, inline=True)
    assert result.data == blob
    # Once the caller has the image, stop telling them how to get it.
    assert "inline=True" not in result.content


async def test_inline_bytes_stay_out_of_the_json_payload(served):
    """`data` must never reach to_dict() — a JSON tool response would
    otherwise carry the whole resource."""
    served(_png(4, 4), "image/png")
    result = await fetch_page("https://x.example/a.png", force_refresh=True, inline=True)
    payload = result.to_dict()
    assert "data" not in payload
    assert payload["media_type"] == "image/png"
    assert payload["width"] == 4


async def test_content_type_routes_extension_less_urls(served):
    """A download endpoint with no extension is still an image."""
    served(_png(2, 2), "image/png")
    result = await fetch_page("https://x.example/download?id=9", force_refresh=True)
    assert result.method == "asset"


async def test_opaque_binary_is_described_without_dimensions(served):
    served(b"\x00\x01\x02\x03" * 100, "application/octet-stream")
    result = await fetch_page("https://x.example/blob", force_refresh=True)
    assert result.method == "asset"
    assert result.width is None
    payload = result.to_dict()
    assert "width" not in payload


async def test_sha256_is_stable_for_identical_bytes(served):
    blob = _png(3, 3)
    served(blob, "image/png")
    first = await fetch_page("https://x.example/a.png", force_refresh=True)
    second = await fetch_page("https://x.example/b.png", force_refresh=True)
    assert first.sha256 == second.sha256


async def test_text_resources_are_unaffected(served):
    """Regression guard: the asset path must not swallow ordinary pages."""
    served(b"<html><body><h1>Hello</h1><p>Body text here.</p></body></html>", "text/html")
    result = await fetch_page("https://x.example/page.html", force_refresh=True)
    assert result.method != "asset"
    assert result.media_type == ""
