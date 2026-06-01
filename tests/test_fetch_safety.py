"""Safety + correctness tests for the fetch/document/structured stack.

Covers (audit item ids in brackets):
  * SSRF guard on the three remote-GET helpers + per-redirect-hop checks  [#1]
  * Response-size caps (Content-Length up front + streaming abort)        [#11]
  * read_document local-file sandbox (opt-in document_root)               [#2]
  * PDF / docx decompression-bomb caps + truncated flag                   [#12]
  * structured: 403/503 bot-block reaches meta_fallback (no raise)        [#6]
  * structured: pathological HTML doesn't escape extruct.extract          [A5]
  * documents._slice returned_chars == SOURCE chars consumed              [#8]
  * documents._slice negative length / start past EOF semantics           [#19]
  * fetcher non-html content-type returns raw body verbatim               [#10]
  * fetcher._extract parses HTML once, output unchanged                   [#17]
  * browser._ensure does not leak the Playwright driver on launch failure [#9]
"""
from __future__ import annotations

import pytest

from search_mcp import config
from search_mcp.url_safety import UnsafeURLError


# --------------------------------------------------------------------------- #
# Helpers: a fake streaming response/client usable for curl_cffi + httpx mocks #
# --------------------------------------------------------------------------- #
class _CurlResp:
    """Mimics a curl_cffi streaming Response (stream=True)."""

    def __init__(self, *, status=200, headers=None, body=b"", encoding="utf-8"):
        self.status_code = status
        self.headers = headers or {}
        self._body = body
        self.encoding = encoding
        self.closed = False
        self.body_consumed = False

    def raise_for_status(self):
        if self.status_code >= 400:
            raise RuntimeError(f"HTTP {self.status_code}")

    async def aiter_content(self, *a, **k):
        self.body_consumed = True
        # chunk it so the streaming guard sees multiple chunks
        for i in range(0, len(self._body), 1024):
            yield self._body[i : i + 1024]

    async def aclose(self):
        self.closed = True


class _CurlSession:
    """Mimics curl_cffi AsyncSession; ``responses`` is a list keyed by call order."""

    def __init__(self, responses):
        self._responses = list(responses)
        self.requested = []

    async def __aenter__(self):
        return self

    async def __aexit__(self, *a):
        return None

    async def get(self, url, *, stream=False, **kw):
        self.requested.append(url)
        return self._responses.pop(0)


# --------------------------------------------------------------------------- #
# #1 SSRF — the three helpers reject blocked URLs before connecting           #
# --------------------------------------------------------------------------- #
@pytest.mark.parametrize(
    "url",
    [
        "http://169.254.169.254/latest/meta-data/",
        "http://127.0.0.1:8080/",
        "http://10.0.0.1/",
    ],
)
@pytest.mark.asyncio
async def test_http_fetch_blocks_ssrf_before_connect(monkeypatch, url):
    from search_mcp import fetcher

    # If the guard fails, this would open a real socket. Make sure it never does.
    def _boom(*a, **k):
        raise AssertionError("AsyncSession must not be constructed for blocked URL")

    monkeypatch.setattr(fetcher, "AsyncSession", _boom)
    monkeypatch.setattr(config.settings, "allow_private_hosts", False)

    with pytest.raises(UnsafeURLError):
        await fetcher._http_fetch(url)


@pytest.mark.parametrize(
    "url",
    [
        "http://169.254.169.254/latest/meta-data/",
        "http://127.0.0.1/",
        "http://10.0.0.1/",
    ],
)
@pytest.mark.asyncio
async def test_read_remote_blocks_ssrf_before_connect(monkeypatch, url):
    from search_mcp import documents

    def _boom(*a, **k):
        raise AssertionError("httpx.AsyncClient must not be constructed for blocked URL")

    monkeypatch.setattr(documents.httpx, "AsyncClient", _boom)
    monkeypatch.setattr(config.settings, "allow_private_hosts", False)

    with pytest.raises(UnsafeURLError):
        await documents._read_remote(url)


@pytest.mark.parametrize(
    "url",
    [
        "http://169.254.169.254/",
        "http://127.0.0.1/",
        "http://10.0.0.1/",
    ],
)
@pytest.mark.asyncio
async def test_extract_structured_blocks_ssrf_before_connect(monkeypatch, url):
    from search_mcp import structured

    def _boom(*a, **k):
        raise AssertionError("httpx.AsyncClient must not be constructed for blocked URL")

    monkeypatch.setattr(structured.httpx, "AsyncClient", _boom)
    monkeypatch.setattr(config.settings, "allow_private_hosts", False)

    with pytest.raises(UnsafeURLError):
        await structured.extract_structured(url)


@pytest.mark.asyncio
async def test_non_http_scheme_blocked(monkeypatch):
    from search_mcp import fetcher

    monkeypatch.setattr(config.settings, "allow_private_hosts", False)
    for bad in ("file:///etc/passwd", "ftp://example.com/x", "gopher://x"):
        with pytest.raises(UnsafeURLError):
            await fetcher._http_fetch(bad)


@pytest.mark.asyncio
async def test_http_fetch_blocks_redirect_to_private_ip(monkeypatch):
    """A 302 -> private IP must be blocked on the hop, never followed."""
    from search_mcp import fetcher

    monkeypatch.setattr(config.settings, "allow_private_hosts", False)

    redirect = _CurlResp(status=302, headers={"location": "http://127.0.0.1/secret"})
    # If the guard fails, this second (terminal) response would be returned.
    terminal = _CurlResp(status=200, headers={"content-type": "text/html"}, body=b"<html>leak</html>")
    session = _CurlSession([redirect, terminal])

    monkeypatch.setattr(fetcher, "AsyncSession", lambda *a, **k: session)

    with pytest.raises(UnsafeURLError):
        await fetcher._http_fetch("https://example.com/start")
    # We requested only the first URL; the private-IP hop was refused.
    assert session.requested == ["https://example.com/start"]
    assert redirect.closed is True
    # The terminal (leak) response was never consumed.
    assert terminal.body_consumed is False


@pytest.mark.asyncio
async def test_http_fetch_follows_safe_redirect(monkeypatch):
    """A redirect to another public host IS followed (manual redirect works)."""
    from search_mcp import fetcher

    monkeypatch.setattr(config.settings, "allow_private_hosts", False)
    redirect = _CurlResp(status=301, headers={"location": "https://example.org/final"})
    terminal = _CurlResp(
        status=200, headers={"content-type": "text/html"}, body=b"<html><body>ok</body></html>"
    )
    session = _CurlSession([redirect, terminal])
    monkeypatch.setattr(fetcher, "AsyncSession", lambda *a, **k: session)

    ctype, text = await fetcher._http_fetch("https://example.com/start")
    assert "ok" in text
    assert "html" in ctype
    assert session.requested == ["https://example.com/start", "https://example.org/final"]


# --------------------------------------------------------------------------- #
# #11 Response-size caps                                                       #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_oversized_content_length_rejected_up_front(monkeypatch):
    from search_mcp import fetcher

    monkeypatch.setattr(config.settings, "allow_private_hosts", False)
    monkeypatch.setattr(config.settings, "max_response_bytes", 1000)
    # Declares 5000 bytes but we should refuse before streaming the body.
    resp = _CurlResp(
        status=200,
        headers={"content-type": "text/html", "content-length": "5000"},
        body=b"x" * 5000,
    )
    session = _CurlSession([resp])
    monkeypatch.setattr(fetcher, "AsyncSession", lambda *a, **k: session)

    with pytest.raises(fetcher.MaxBytesExceededError):
        await fetcher._http_fetch("https://example.com/big")
    assert resp.body_consumed is False  # never streamed the body


@pytest.mark.asyncio
async def test_streaming_aborts_when_body_exceeds_cap(monkeypatch):
    """Server lies / omits Content-Length: the streaming guard still aborts."""
    from search_mcp import fetcher

    monkeypatch.setattr(config.settings, "allow_private_hosts", False)
    monkeypatch.setattr(config.settings, "max_response_bytes", 2000)
    # No content-length header; body is 10k -> must abort mid-stream.
    resp = _CurlResp(status=200, headers={"content-type": "text/html"}, body=b"y" * 10_000)
    session = _CurlSession([resp])
    monkeypatch.setattr(fetcher, "AsyncSession", lambda *a, **k: session)

    with pytest.raises(fetcher.MaxBytesExceededError):
        await fetcher._http_fetch("https://example.com/lie")


@pytest.mark.asyncio
async def test_accumulate_capped_aborts_before_full_buffer():
    from search_mcp import fetcher

    async def gen():
        # 6 chunks of 1000 bytes = 6000 total; cap below that should abort early.
        for _ in range(6):
            yield b"z" * 1000

    orig = config.settings.max_response_bytes
    config.settings.max_response_bytes = 2500
    try:
        with pytest.raises(fetcher.MaxBytesExceededError):
            await fetcher._accumulate_capped(gen())
    finally:
        config.settings.max_response_bytes = orig


# --------------------------------------------------------------------------- #
# #2 Local-file sandbox                                                        #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_local_read_disabled_by_default(monkeypatch):
    from search_mcp import documents

    monkeypatch.setattr(config.settings, "document_root", None)
    with pytest.raises(PermissionError) as exc:
        await documents.read_document("/etc/passwd")
    assert "SEARCH_MCP_DOCUMENT_ROOT" in str(exc.value)


@pytest.mark.asyncio
async def test_local_read_file_scheme_rejected(monkeypatch, tmp_path):
    from search_mcp import documents

    monkeypatch.setattr(config.settings, "document_root", tmp_path)
    with pytest.raises(ValueError):
        await documents.read_document("file:///etc/passwd")


@pytest.mark.asyncio
async def test_local_read_traversal_blocked(monkeypatch, tmp_path):
    from search_mcp import documents

    root = tmp_path / "sandbox"
    root.mkdir()
    monkeypatch.setattr(config.settings, "document_root", root)
    # Traversal escaping the root must be refused.
    with pytest.raises(PermissionError):
        await documents.read_document("../../etc/passwd")
    # Absolute path outside the sandbox also refused.
    with pytest.raises(PermissionError):
        await documents.read_document("/etc/passwd")


@pytest.mark.asyncio
async def test_local_read_inside_root_succeeds(monkeypatch, tmp_path):
    from search_mcp import documents

    root = tmp_path / "sandbox"
    root.mkdir()
    f = root / "note.txt"
    f.write_text("hello sandbox", encoding="utf-8")
    monkeypatch.setattr(config.settings, "document_root", root)

    out = await documents.read_document(str(f))
    assert out.content == "hello sandbox"
    # Relative path resolves against the root, not the CWD.
    out2 = await documents.read_document("note.txt")
    assert out2.content == "hello sandbox"


# --------------------------------------------------------------------------- #
# #12 PDF / docx decompression-bomb caps                                      #
# --------------------------------------------------------------------------- #
class _FakePage:
    def __init__(self, text):
        self._text = text

    def extract_text(self):
        return self._text


class _FakeReader:
    def __init__(self, blob):  # signature matches PdfReader(io.BytesIO(blob))
        self.pages = [_FakePage(f"page {i} text body") for i in range(500)]
        self.metadata = None


def test_parse_pdf_caps_pages(monkeypatch):
    from search_mcp import documents

    monkeypatch.setattr(documents, "PdfReader", _FakeReader)
    monkeypatch.setattr(config.settings, "max_pdf_pages", 10)
    monkeypatch.setattr(config.settings, "max_document_chars", 10_000_000)

    title, text, total, truncated = documents._parse_pdf(b"ignored")
    assert total == 500  # reports the true page count
    assert truncated is True
    # Only the first 10 pages were emitted.
    assert "## Page 10" in text
    assert "## Page 11" not in text


def test_parse_pdf_caps_chars(monkeypatch):
    from search_mcp import documents

    monkeypatch.setattr(documents, "PdfReader", _FakeReader)
    monkeypatch.setattr(config.settings, "max_pdf_pages", 1000)
    monkeypatch.setattr(config.settings, "max_document_chars", 50)

    title, text, total, truncated = documents._parse_pdf(b"ignored")
    assert truncated is True
    assert len(text) < 10_000  # stopped early, nowhere near 500 pages


class _FakeParagraph:
    def __init__(self, text):
        self.text = text
        self.style = None


class _FakeDocxDoc:
    def __init__(self, blob):
        self.paragraphs = [_FakeParagraph("word " * 50) for _ in range(1000)]
        self.tables = []


def test_parse_docx_caps_chars(monkeypatch):
    from search_mcp import documents

    monkeypatch.setattr(documents, "DocxDocument", _FakeDocxDoc)
    monkeypatch.setattr(config.settings, "max_document_chars", 200)

    text, truncated = documents._parse_docx(b"ignored")
    assert truncated is True
    assert len(text) < 100_000  # did not iterate all 1000 paragraphs fully


def test_parse_docx_small_not_truncated(monkeypatch):
    from search_mcp import documents

    class _Small:
        def __init__(self, blob):
            self.paragraphs = [_FakeParagraph("hi"), _FakeParagraph("there")]
            self.tables = []

    monkeypatch.setattr(documents, "DocxDocument", _Small)
    monkeypatch.setattr(config.settings, "max_document_chars", 2_000_000)
    text, truncated = documents._parse_docx(b"ignored")
    assert truncated is False
    assert "hi" in text and "there" in text


# --------------------------------------------------------------------------- #
# #6 structured: bot-block (403/503) reaches meta_fallback, no raise          #
# --------------------------------------------------------------------------- #
class _HttpxStream:
    def __init__(self, *, status, headers, body, encoding="utf-8"):
        self.status_code = status
        self.headers = headers
        self._body = body
        self.encoding = encoding

    async def __aenter__(self):
        return self

    async def __aexit__(self, *a):
        return None

    async def aiter_bytes(self):
        yield self._body


class _HttpxClient:
    def __init__(self, stream_resp):
        self._stream_resp = stream_resp

    async def __aenter__(self):
        return self

    async def __aexit__(self, *a):
        return None

    def stream(self, method, url):
        return self._stream_resp


@pytest.mark.asyncio
async def test_extract_structured_403_returns_hint_no_raise(monkeypatch):
    from search_mcp import structured

    monkeypatch.setattr(config.settings, "allow_private_hosts", False)
    html = b"<html><head><meta name='description' content='blocked shell'></head><body></body></html>"
    resp = _HttpxStream(status=403, headers={"content-type": "text/html"}, body=html)
    monkeypatch.setattr(
        structured.httpx, "AsyncClient", lambda *a, **k: _HttpxClient(resp)
    )

    payload = await structured.extract_structured("https://example.com/blocked")
    # No raise; we reach the fallback/hint path.
    assert "hint" in payload
    assert "403" in payload["hint"]
    assert payload["meta_fallback"].get("description") == "blocked shell"


# --------------------------------------------------------------------------- #
# A5 structured: pathological HTML doesn't escape extruct.extract             #
# --------------------------------------------------------------------------- #
def test_extract_structured_from_html_extruct_raises_falls_through(monkeypatch):
    from search_mcp import structured

    def _boom(*a, **k):
        raise RuntimeError("extruct exploded on pathological input")

    monkeypatch.setattr(structured.extruct, "extract", _boom)
    html = "<html><head><meta name='author' content='X'></head><body></body></html>"
    payload = structured.extract_structured_from_html(html, "https://example.com/x")
    # Graceful: empty syntax lists + hint, no exception.
    assert payload["json_ld"] == []
    assert "hint" in payload
    assert payload["meta_fallback"].get("author") == "X"


def test_extract_structured_from_html_get_base_url_raises_falls_through(monkeypatch):
    from search_mcp import structured

    def _boom(*a, **k):
        raise RuntimeError("get_base_url exploded")

    monkeypatch.setattr(structured, "get_base_url", _boom)
    html = "<html><body><p>hi</p></body></html>"
    payload = structured.extract_structured_from_html(html, "https://example.com/x")
    assert payload["json_ld"] == []
    assert "hint" in payload


# --------------------------------------------------------------------------- #
# #8 _slice returned_chars == SOURCE chars consumed                           #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_slice_returned_chars_is_source_consumed(monkeypatch, tmp_path):
    from search_mcp import documents

    monkeypatch.setattr(config.settings, "document_root", tmp_path)
    monkeypatch.setattr(config.settings, "max_content_chars", 120)

    # Build a doc that forces a SOFT truncation (paragraph boundary inside the
    # window) so smart_truncate appends a marker.
    body = ("alpha beta gamma delta. " * 4) + "\n\n" + ("more text here. " * 40)
    p = tmp_path / "doc.txt"
    p.write_text(body, encoding="utf-8")

    first = await documents.read_document(str(p), start=0)
    assert first.truncated is True
    # The marker is NOT counted in returned_chars: paginating by returned_chars
    # lands exactly where the displayed content (minus marker) ended.
    second = await documents.read_document(str(p), start=first.returned_chars)

    # Reconstruct the source: displayed-content-without-marker for read 1, then
    # the remainder starting at returned_chars. They must join with no gap.
    displayed = first.content
    for marker in ("\n\n[…truncated]", " …"):
        if displayed.endswith(marker):
            displayed = displayed[: -len(marker)]
            break
    # The displayed (non-marker) prefix must equal the source up to returned_chars.
    assert displayed == body[: first.returned_chars], "marker chars leaked into returned_chars"
    # And the second read picks up exactly from there with no skipped chars.
    assert second.content.startswith(body[first.returned_chars : first.returned_chars + 20])


# --------------------------------------------------------------------------- #
# #19 _slice negative length + start past EOF                                 #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_slice_negative_length_raises(monkeypatch, tmp_path):
    from search_mcp import documents

    monkeypatch.setattr(config.settings, "document_root", tmp_path)
    p = tmp_path / "s.txt"
    p.write_text("0123456789", encoding="utf-8")
    with pytest.raises(ValueError):
        await documents.read_document(str(p), start=10, length=-5)


@pytest.mark.asyncio
async def test_slice_start_past_eof_reports_clamped_start(monkeypatch, tmp_path):
    from search_mcp import documents

    monkeypatch.setattr(config.settings, "document_root", tmp_path)
    p = tmp_path / "short.txt"
    p.write_text("0123456789", encoding="utf-8")  # 10 chars
    out = await documents.read_document(str(p), start=999_999)
    # start is clamped to total length, not echoed back unclamped.
    assert out.start == 10
    assert out.returned_chars == 0
    assert out.content == ""
    # Nothing was withheld (we read from EOF to EOF), so not truncated.
    assert out.truncated is False


# --------------------------------------------------------------------------- #
# #10 fetcher non-html content-type returns raw body verbatim                 #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_fetch_page_json_returned_verbatim(monkeypatch):
    from search_mcp import fetcher

    monkeypatch.setattr(config.settings, "allow_private_hosts", False)
    json_body = '{"answer": 42, "nested": {"x": [1, 2, 3]}}'

    async def fake_http_fetch(url):
        return "application/json", json_body

    monkeypatch.setattr(fetcher, "_http_fetch", fake_http_fetch)

    # _extract must NOT be called for JSON.
    def _no_extract(*a, **k):
        raise AssertionError("_extract must be skipped for non-html content-type")

    monkeypatch.setattr(fetcher, "_extract", _no_extract)

    # Stub the cache so we don't touch sqlite.
    class _Cache:
        async def get_page(self, url):
            return None

        async def put_page(self, *a, **k):
            return None

    monkeypatch.setattr(fetcher, "cache", _Cache())

    # render="http" pins the HTTP path (a short body would otherwise trip the
    # auto browser-fallback heuristic and hit the network).
    result = await fetcher.fetch_page(
        "https://example.com/data.json", render="http", force_refresh=True
    )
    assert result.content == json_body  # verbatim, not trafilatura output


@pytest.mark.asyncio
async def test_fetch_page_html_still_extracted(monkeypatch):
    from search_mcp import fetcher

    monkeypatch.setattr(config.settings, "allow_private_hosts", False)
    html = (
        "<html><body><article><h1>Heading Title</h1>"
        "<p>This is a sufficiently long body paragraph so that the auto render "
        "heuristic treats the HTTP body as complete and does not fall back to "
        "the browser path while we verify extraction runs for markup.</p>"
        "</article></body></html>"
    )

    async def fake_http_fetch(url):
        return "text/html", html

    monkeypatch.setattr(fetcher, "_http_fetch", fake_http_fetch)

    called = {"n": 0}
    real_extract = fetcher._extract

    def counting_extract(h, u):
        called["n"] += 1
        return real_extract(h, u)

    monkeypatch.setattr(fetcher, "_extract", counting_extract)

    class _Cache:
        async def get_page(self, url):
            return None

        async def put_page(self, *a, **k):
            return None

    monkeypatch.setattr(fetcher, "cache", _Cache())

    result = await fetcher.fetch_page(
        "https://example.com/page", render="http", force_refresh=True
    )
    assert called["n"] == 1  # html DID go through extraction
    assert "Heading Title" in result.content


# --------------------------------------------------------------------------- #
# #17 _extract parses HTML once, output unchanged on a fixture                #
# --------------------------------------------------------------------------- #
_FIXTURE_HTML = """\
<!DOCTYPE html>
<html>
<head><title>Fixture Title</title></head>
<body>
  <article>
    <h1>Heading One</h1>
    <p>This is a sufficiently long paragraph of body text so that trafilatura's
    content-extraction heuristics consider it the main article content and emit
    it into the markdown output without discarding it as boilerplate.</p>
  </article>
</body>
</html>
"""


def test_extract_output_stable_and_parses_once(monkeypatch):
    from search_mcp import fetcher

    # load_html should be called exactly once (single parse) for the shared tree.
    import trafilatura

    calls = {"load": 0}
    real_load = trafilatura.load_html

    def counting_load(html):
        calls["load"] += 1
        return real_load(html)

    monkeypatch.setattr(fetcher.trafilatura, "load_html", counting_load)

    title, md, author, date, sitename = fetcher._extract(_FIXTURE_HTML, "https://example.com/a")
    assert calls["load"] == 1, "HTML should be parsed exactly once"
    assert "Heading One" in md
    assert "body text" in md
    # Title comes through (from metadata or fallback).
    assert title


def test_extract_output_matches_string_path():
    """The tree-based path yields the same content as a plain string call."""
    import trafilatura

    from search_mcp import fetcher

    _, md_ours, _, _, _ = fetcher._extract(_FIXTURE_HTML, "https://example.com/a")
    md_string = (
        trafilatura.extract(
            _FIXTURE_HTML,
            url="https://example.com/a",
            output_format="markdown",
            include_links=True,
            include_tables=True,
            favor_precision=True,
        )
        or ""
    ).strip()
    assert md_ours == md_string


# --------------------------------------------------------------------------- #
# #9 browser._ensure does not leak the Playwright driver on launch failure    #
# --------------------------------------------------------------------------- #
@pytest.mark.asyncio
async def test_browser_ensure_no_driver_leak_on_launch_failure(monkeypatch, tmp_path):
    from search_mcp import browser

    monkeypatch.setattr(config.settings, "cache_dir", tmp_path)

    stop_calls = {"n": 0}

    class _FakeChromium:
        async def launch_persistent_context(self, *a, **k):
            raise RuntimeError("launch failed (both channel and bundled)")

    class _FakePlaywright:
        def __init__(self):
            self.chromium = _FakeChromium()

        async def stop(self):
            stop_calls["n"] += 1

    class _FakeAsyncPlaywright:
        async def start(self):
            return _FakePlaywright()

    monkeypatch.setattr(browser, "async_playwright", lambda: _FakeAsyncPlaywright())

    pool = browser.BrowserPool()

    with pytest.raises(RuntimeError):
        await pool._ensure()
    assert pool._playwright is None  # driver torn down, not leaked
    assert stop_calls["n"] == 1

    # Second call must START a fresh driver (not reuse a leaked one) and also
    # tear it down on failure.
    with pytest.raises(RuntimeError):
        await pool._ensure()
    assert pool._playwright is None
    assert stop_calls["n"] == 2


@pytest.mark.asyncio
async def test_browser_ensure_resets_ctx_on_init_script_failure(monkeypatch, tmp_path):
    """If add_init_script fails, the half-built ctx is closed so retry is clean."""
    from search_mcp import browser

    monkeypatch.setattr(config.settings, "cache_dir", tmp_path)
    closed = {"n": 0}
    stopped = {"n": 0}

    class _FakeCtx:
        async def add_init_script(self, script):
            raise RuntimeError("init script failed")

        async def close(self):
            closed["n"] += 1

    class _FakeChromium:
        async def launch_persistent_context(self, *a, **k):
            return _FakeCtx()

    class _FakePlaywright:
        def __init__(self):
            self.chromium = _FakeChromium()

        async def stop(self):
            stopped["n"] += 1

    class _FakeAsyncPlaywright:
        async def start(self):
            return _FakePlaywright()

    monkeypatch.setattr(browser, "async_playwright", lambda: _FakeAsyncPlaywright())

    pool = browser.BrowserPool()
    with pytest.raises(RuntimeError):
        await pool._ensure()
    assert pool._ctx is None  # half-built context discarded
    assert closed["n"] == 1
    assert pool._playwright is None  # driver also torn down
    assert stopped["n"] == 1
