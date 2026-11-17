"""Deterministic, offline follow-up checks for backend resource budgets."""
from __future__ import annotations

import asyncio
import io
import socket
import stat
import threading
import zipfile
from types import SimpleNamespace

import pytest

from search_mcp import documents, downloads, httpfetch, url_safety
from search_mcp.config import settings


async def test_async_fake_ip_canaries_leave_event_loop_responsive(monkeypatch):
    loop = asyncio.get_running_loop()
    loop_thread = threading.get_ident()
    heartbeat = threading.Event()
    entered = asyncio.Event()
    seen = []

    def answer(host, port=None, **kwargs):
        seen.append(host)
        assert threading.get_ident() != loop_thread, "canary DNS blocked the event loop"
        loop.call_soon_threadsafe(entered.set)
        assert heartbeat.wait(2), "heartbeat could not run during canary lookup"
        return [(socket.AF_INET, socket.SOCK_STREAM, socket.IPPROTO_TCP, "",
                 ("198.18.0.2", port or 0))]

    async def lookup(host, port=None, **kwargs):
        return [(socket.AF_INET, socket.SOCK_STREAM, socket.IPPROTO_TCP, "",
                 ("198.18.0.1", port or 0))]

    monkeypatch.setattr(loop, "getaddrinfo", lookup)
    monkeypatch.setattr(socket, "getaddrinfo", answer)
    task = asyncio.create_task(url_safety.assert_url_allowed_async("https://synthetic.example/"))

    async def beat():
        await entered.wait()
        heartbeat.set()

    progress = asyncio.create_task(beat())
    try:
        assert await asyncio.wait_for(task, 3)
        await progress
        assert seen == list(url_safety._CANARY_HOSTS)
        # Another host reuses the canary verdict, not the host-validation cache.
        await url_safety.assert_url_allowed_async("https://another.example/")
        assert seen == list(url_safety._CANARY_HOSTS)
    finally:
        heartbeat.set()
        progress.cancel()
        await asyncio.gather(progress, return_exceptions=True)


def _zip_bytes(sizes):
    output = io.BytesIO()
    with zipfile.ZipFile(output, "w", zipfile.ZIP_DEFLATED) as archive:
        for index, size in enumerate(sizes):
            archive.writestr(f"member{index}.xml", b"x" * size)
    return output.getvalue()


@pytest.mark.parametrize("fmt", ["docx", "xlsx", "pptx", "epub"])
@pytest.mark.parametrize("sizes", [(2048,), (600, 600)])
def test_zip_expansion_budget_precedes_loader_and_html_fallback(monkeypatch, fmt, sizes):
    import openpyxl
    import pptx
    from ebooklib import epub

    blob = _zip_bytes(sizes)
    assert len(blob) < 1024
    monkeypatch.setattr(settings, "max_response_bytes", 1024)

    def forbidden(*args, **kwargs):
        pytest.fail("budget refusal must precede eager loading or HTML fallback")

    for module, attr in [(documents, "DocxDocument"), (openpyxl, "load_workbook"),
                         (pptx, "Presentation"), (epub, "read_epub"),
                         (documents, "_parse_html")]:
        monkeypatch.setattr(module, attr, forbidden)
    with pytest.raises(ValueError, match="(?i)(expanded|budget|limit)"):
        documents._parse_blob(blob, "", f"test.{fmt}", fmt)


def test_zip_exact_expansion_budget_is_accepted(monkeypatch):
    monkeypatch.setattr(settings, "max_response_bytes", 1024)
    documents._check_zip_budget(_zip_bytes((512, 512)))


@pytest.mark.parametrize("row", [b"\n", b",\n", b"   ,   \n", b'"|||",\n'])
def test_csv_blank_and_escaped_rows_have_bounded_rendering(monkeypatch, row):
    monkeypatch.setattr(settings, "max_document_chars", 100)
    text, truncated = documents._parse_csv(b"a,b\n" + row * 2000)
    assert truncated
    assert len(text) <= 100


def test_csv_stops_reading_rows_when_render_budget_is_spent(monkeypatch):
    import csv

    monkeypatch.setattr(settings, "max_document_chars", 100)
    original = csv.reader
    visited = 0

    def bounded(*args, **kwargs):
        nonlocal visited
        for row in original(*args, **kwargs):
            visited += 1
            assert visited <= 20, "parser read the entire CSV before truncating output"
            yield row

    monkeypatch.setattr(csv, "reader", bounded)
    text, truncated = documents._parse_csv(b"a,b\n" + b",\n" * 20000)

    assert truncated
    assert len(text) <= 100
    assert visited < 20


def test_clipped_xlsx_columns_do_not_hide_later_sheets(monkeypatch):
    from openpyxl import Workbook

    workbook = Workbook()
    workbook.active.title = "Wide"
    workbook.active.cell(1, 31, "outside visible columns")
    workbook.create_sheet("Second").cell(1, 1, "second marker")
    workbook.create_sheet("Third").cell(1, 1, "third marker")
    data = io.BytesIO()
    workbook.save(data)
    monkeypatch.setattr(settings, "max_document_chars", 10000)

    text, truncated = documents._parse_xlsx(data.getvalue())

    assert truncated
    assert "second marker" in text
    assert "third marker" in text


def test_local_special_file_refused_without_opening(monkeypatch):
    class Pipe:
        def stat(self):
            return SimpleNamespace(st_mode=stat.S_IFIFO, st_size=0)

        def open(self, *args):
            pytest.fail("a named pipe must not be opened")

    with pytest.raises(ValueError, match="regular file"):
        documents._read_local_capped(Pipe())


async def test_local_file_io_runs_off_loop_and_bounds_growing_files(monkeypatch):
    loop_thread = threading.get_ident()
    reads = []
    monkeypatch.setattr(settings, "max_response_bytes", 64)

    class GrowingFile:
        def stat(self):
            assert threading.get_ident() != loop_thread
            return SimpleNamespace(st_mode=stat.S_IFREG, st_size=1)

        def open(self, mode):
            assert threading.get_ident() != loop_thread
            return self

        def __enter__(self):
            return self

        def __exit__(self, *args):
            return False

        def read(self, limit):
            reads.append(limit)
            return b"x" * limit

    monkeypatch.setattr(documents, "_resolve_local_path", lambda source: GrowingFile())
    with pytest.raises(ValueError, match="(?i)(budget|limit|cap)"):
        await documents.read_document("growing.txt")
    assert reads == [65]


@pytest.mark.parametrize("name, media", [
    ("a." + "z" * 500, ""),
    ("a" * 500 + "." + "z" * 500, ""),
    ("a" * 500, "image/png"),
])
def test_download_component_cap_includes_inferred_extension(monkeypatch, tmp_path, name, media):
    monkeypatch.setattr(settings, "download_dir", tmp_path)
    path = downloads.save(f"https://example.com/{name}", b"x", media)
    assert len(path.name) <= downloads._MAX_NAME + 9
    assert path.read_bytes() == b"x"


@pytest.mark.parametrize("encoding", ["idna", "undefined", "base64_codec", "not_a_codec"])
def test_unsupported_codecs_use_utf8_replacement(encoding):
    assert httpfetch._decode_body(b"hello\xff", f"text/plain; charset={encoding}") == "hello�"
