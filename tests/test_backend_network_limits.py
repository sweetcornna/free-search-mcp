"""Offline regressions for URL aliases and bounded document/download processing."""
from __future__ import annotations

import io
import zipfile

import pytest

from search_mcp import documents, downloads, httpfetch, url_safety
from search_mcp.config import settings


@pytest.mark.parametrize("host", ["127.1", "2130706433", "0x7f000001", "%31%32%37.0.0.1", "%6cocalhost", "localhost。", "127.0.0.1."])
async def test_proxied_private_aliases_are_refused(monkeypatch, host):
    monkeypatch.setattr(url_safety.net, "proxy_url", lambda: "http://proxy.example:3128")
    with pytest.raises(url_safety.UnsafeURLError):
        await url_safety.assert_url_allowed_async(f"http://{host}/")
    with pytest.raises(url_safety.UnsafeURLError):
        url_safety.assert_url_allowed_offline(f"http://{host}/")


def test_idna_public_host_and_explicit_private_opt_in(monkeypatch):
    assert url_safety.assert_url_allowed_offline("https://bücher.example/")
    monkeypatch.setattr(settings, "allow_private_hosts", True)
    assert url_safety.assert_url_allowed_offline("http://127.1/")


def test_unsuitable_text_codec_falls_back():
    assert httpfetch._decode_body(b"hello", "text/plain; charset=idna") == "hello"
    assert "hello" in httpfetch._decode_body(b"<meta charset='idna'>hello", "text/html")


def test_long_filename_extension_is_bounded():
    name = downloads.safe_filename("https://example.com/a." + "z" * 500, blob=b"x")
    assert len(name) <= downloads._MAX_NAME + 9


def test_whitespace_pagination_always_advances(monkeypatch):
    monkeypatch.setattr(settings, "max_content_chars", 120)
    text = " " * 200 + "actual content"
    offset = 0
    for _ in range(4):
        content, truncated, returned, _ = documents._slice(text, offset, None)
        if not truncated:
            assert "actual content" in content
            break
        assert returned > 0
        offset += returned
    else:
        pytest.fail("pagination never reached EOF")


def test_empty_csv_rows_cannot_evade_document_budget(monkeypatch):
    monkeypatch.setattr(settings, "max_document_chars", 100)
    text, truncated = documents._parse_csv(b"a,b\n" + b",\n" * 20000)
    assert truncated
    assert len(text) <= 100


def _repack(blob, replacements):
    output = io.BytesIO()
    with zipfile.ZipFile(io.BytesIO(blob)) as source, zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as target:
        for info in source.infolist():
            data = source.read(info.filename)
            target.writestr(info.filename, replacements.get(info.filename, lambda b: b)(data))
    return output.getvalue()


def test_docx_heading_multiplier_is_bounded(monkeypatch):
    from docx import Document
    from docx.enum.style import WD_STYLE_TYPE

    doc = Document()
    doc.styles.add_style("Heading 100000", WD_STYLE_TYPE.PARAGRAPH)
    doc.add_paragraph("small", "Heading 100000")
    blob = io.BytesIO()
    doc.save(blob)
    monkeypatch.setattr(settings, "max_document_chars", 100)
    text, _ = documents._parse_docx(blob.getvalue())
    assert len(text) < 100


def test_docx_expansion_is_refused_before_parser(monkeypatch):
    from docx import Document

    doc = Document()
    doc.add_paragraph("small")
    blob = io.BytesIO()
    doc.save(blob)
    expanded = _repack(blob.getvalue(), {"word/document.xml": lambda _: b"x" * 2000000})
    monkeypatch.setattr(settings, "max_response_bytes", 1000000)
    with pytest.raises(ValueError, match="(?i)(budget|limit|expanded)"):
        documents._parse_blob(expanded, "", "a.docx", "docx")


def test_sparse_xlsx_does_not_expand_unbounded_grid(monkeypatch):
    from openpyxl import Workbook
    from openpyxl.worksheet._read_only import ReadOnlyWorksheet

    wb = Workbook()
    wb.active["A1"] = "one"
    blob = io.BytesIO()
    wb.save(blob)
    sparse = _repack(blob.getvalue(), {"xl/worksheets/sheet1.xml": lambda b: b.replace(b"A1:A1", b"A1:XFD1048576").replace(b'r="1"', b'r="1048576"').replace(b'r="A1"', b'r="XFD1048576"')})
    original = ReadOnlyWorksheet.iter_rows

    def bounded(self, *args, **kwargs):
        assert kwargs.get("max_col", 16384) <= 30
        assert kwargs.get("max_row", 1048576) <= 10000
        yield from original(self, *args, **kwargs)

    monkeypatch.setattr(ReadOnlyWorksheet, "iter_rows", bounded)
    _, truncated = documents._parse_xlsx(sparse)
    assert truncated


def _compress_body(body, encoding):
    import gzip
    import zlib

    if encoding == "gzip-members":
        return gzip.compress(body[:len(body) // 2]) + gzip.compress(body[len(body) // 2:])
    if encoding == "gzip":
        return gzip.compress(body)
    return zlib.compress(body, wbits=-zlib.MAX_WBITS if encoding == "raw-deflate" else zlib.MAX_WBITS)


@pytest.mark.parametrize("encoding", ["gzip", "gzip-members", "deflate", "raw-deflate"])
@pytest.mark.parametrize("chunk_size", [1, 4096])
async def test_compressed_httpx_body_uses_bounded_raw_decoder(monkeypatch, encoding, chunk_size):
    import httpx

    body = b"a normal compressed response\n" * 20
    wire = _compress_body(body, encoding)
    assert len(wire) < len(body)
    # Accept a response exactly at the decoded cap, even across header/trailer
    # boundaries and concatenated gzip members.
    monkeypatch.setattr(settings, "max_response_bytes", len(body))

    class Stream(httpx.AsyncByteStream):
        closed = False

        async def __aiter__(self):
            for index in range(0, len(wire), chunk_size):
                yield wire[index:index + chunk_size]

        async def aclose(self):
            self.closed = True

    def forbidden(*args, **kwargs):
        pytest.fail("httpx's automatic decompressor must never run")

    monkeypatch.setattr(httpx.Response, "_get_content_decoder", forbidden)
    stream = Stream()

    def respond(request):
        assert request.headers["accept-encoding"] == "gzip, deflate"
        return httpx.Response(200, headers={
            "content-encoding": "gzip" if encoding.startswith("gzip") else "deflate",
            "content-type": "text/plain",
            "content-length": str(len(wire)),
        }, stream=stream)

    async with httpx.AsyncClient(**httpfetch.httpx_client_kwargs(), transport=httpx.MockTransport(respond)) as client:
        assert await httpfetch.httpx_stream_capped(client, "https://example.com/", raise_for_status=True) == (200, "text/plain", body)
    assert stream.closed


@pytest.mark.parametrize("encoding", ["gzip", "deflate", "raw-deflate"])
async def test_compressed_httpx_bomb_aborts_with_bounded_expansion(monkeypatch, encoding):
    import zlib

    import httpx

    cap = 1024
    wire = _compress_body(b"x" * 1000000, encoding)
    prefix_size = {"gzip": 24, "deflate": 16, "raw-deflate": 14}[encoding]
    assert len(wire) <= cap
    monkeypatch.setattr(settings, "max_response_bytes", cap)
    original = zlib.decompressobj
    allocations = []

    class MeteredDecoder:
        def __init__(self, *args, **kwargs):
            self.decoder = original(*args, **kwargs)

        def decompress(self, data, max_length=0):
            assert 0 < max_length <= cap + 1 - sum(allocations)
            result = self.decoder.decompress(data, max_length=max_length)
            allocations.append(len(result))
            return result

        def flush(self, *args, **kwargs):
            pytest.fail("zlib.flush length is not an output cap")

        def __getattr__(self, name):
            return getattr(self.decoder, name)

    monkeypatch.setattr(zlib, "decompressobj", MeteredDecoder)

    class Stream(httpx.AsyncByteStream):
        closed = False

        async def __aiter__(self):
            # Ensure the second decoder call uses the remaining budget, not
            # the original cap. This prefix emits some, but not all, output.
            yield wire[:prefix_size]
            yield wire[prefix_size:]
            pytest.fail("must abort before reading another wire chunk")

        async def aclose(self):
            self.closed = True

    stream = Stream()
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda req: httpx.Response(200, headers={"content-encoding": "gzip" if encoding == "gzip" else "deflate"}, stream=stream))) as client:
        with pytest.raises(httpfetch.MaxBytesExceededError):
            await httpfetch.httpx_stream_capped(client, "https://example.com/", raise_for_status=True)
    assert 0 < allocations[0] <= cap
    assert len(allocations) == 2
    assert sum(allocations) == cap + 1
    assert stream.closed


@pytest.mark.parametrize("declared", [False, True])
@pytest.mark.parametrize("over_cap", [False, True])
async def test_compressed_wire_cap_is_independent_of_decoded_size(monkeypatch, declared, over_cap):
    import httpx

    body = b"small"
    wire = _compress_body(body, "gzip")
    cap = len(wire) - int(over_cap)
    assert len(body) < cap
    monkeypatch.setattr(settings, "max_response_bytes", cap)
    visited = []

    class Stream(httpx.AsyncByteStream):
        closed = False

        async def __aiter__(self):
            for byte in wire:
                visited.append(byte)
                yield bytes([byte])
            if over_cap:
                pytest.fail("wire budget must stop before another chunk is read")

        async def aclose(self):
            self.closed = True

    headers = {"content-encoding": "gzip"}
    if declared:
        headers["content-length"] = str(len(wire))
    stream = Stream()
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda req: httpx.Response(200, headers=headers, stream=stream))) as client:
        if over_cap:
            with pytest.raises(httpfetch.MaxBytesExceededError, match="(wire cap|Content-Length)"):
                await httpfetch.httpx_stream_capped(client, "https://example.com/", raise_for_status=True)
        else:
            assert (await httpfetch.httpx_stream_capped(client, "https://example.com/", raise_for_status=True))[2] == body
    assert len(visited) == (0 if declared and over_cap else len(wire))
    assert stream.closed


@pytest.mark.parametrize("encoding", ["br", "zstd", "unknown", "gzip, deflate", "gzip, gzip"])
async def test_unsupported_httpx_encoding_is_refused_before_read(encoding):
    import httpx

    class Stream(httpx.AsyncByteStream):
        closed = False

        async def __aiter__(self):
            pytest.fail("unsupported encoding must not reach a decoder")
            yield b""

        async def aclose(self):
            self.closed = True

    stream = Stream()
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda req: httpx.Response(200, headers={"content-encoding": encoding}, stream=stream))) as client:
        with pytest.raises(httpfetch.FetchError, match="Unsupported.*Encoding"):
            await httpfetch.httpx_stream_capped(client, "https://example.com/", raise_for_status=True)
    assert stream.closed


@pytest.mark.parametrize("encoding", ["gzip", "deflate"])
@pytest.mark.parametrize("damage", ["truncated", "checksum", "trailing", "empty"])
async def test_damaged_compressed_httpx_body_fails_closed(monkeypatch, encoding, damage):
    import httpx

    wire = _compress_body(b"small response", encoding)
    if damage == "truncated":
        wire = wire[:-1]
    elif damage == "checksum":
        wire = wire[:-5] + bytes([wire[-5] ^ 255]) + wire[-4:]
    elif damage == "trailing":
        wire += b"not another compressed member"
    else:
        wire = b""
    monkeypatch.setattr(settings, "max_response_bytes", 1024)

    class Stream(httpx.AsyncByteStream):
        closed = False

        async def __aiter__(self):
            yield wire

        async def aclose(self):
            self.closed = True

    stream = Stream()
    async with httpx.AsyncClient(transport=httpx.MockTransport(lambda req: httpx.Response(200, headers={"content-encoding": encoding}, stream=stream))) as client:
        with pytest.raises(httpfetch.FetchError, match="(?i)(invalid|truncated|unexpected)"):
            await httpfetch.httpx_stream_capped(client, "https://example.com/", raise_for_status=True)
    assert stream.closed


async def test_oversized_local_file_refused_before_read(monkeypatch, tmp_path):
    path = tmp_path / "big.txt"
    path.write_bytes(b"x" * 100)
    monkeypatch.setattr(settings, "document_root", tmp_path)
    monkeypatch.setattr(settings, "max_response_bytes", 50)
    with pytest.raises(ValueError, match="(?i)(budget|limit|cap)"):
        await documents.read_document("big.txt")
