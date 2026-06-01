from __future__ import annotations

import io
import logging
from dataclasses import dataclass
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

import httpx
from docx import Document as DocxDocument
from markdownify import markdownify as html_to_md
from pypdf import PdfReader

from .config import settings
from .net import proxy_url
from .fetcher import (
    _accumulate_capped,
    _check_content_length,
    _resolve_redirect_location,
    _MAX_REDIRECTS,
)
from .formatting import estimate_tokens, smart_truncate
from .url_safety import assert_url_allowed

log = logging.getLogger(__name__)


@dataclass(slots=True)
class DocumentResult:
    source: str
    format: str
    title: str
    content: str
    truncated: bool
    pages: int | None = None
    tokens_estimated: int = 0
    total_chars: int = 0
    start: int = 0
    returned_chars: int = 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "source": self.source,
            "format": self.format,
            "title": self.title,
            "content": self.content,
            "truncated": self.truncated,
            "pages": self.pages,
            "tokens_estimated": self.tokens_estimated,
            "total_chars": self.total_chars,
            "start": self.start,
            "returned_chars": self.returned_chars,
        }


def _detect_format(source: str, content_type: str | None = None) -> str:
    s = source.lower()
    if s.endswith(".pdf") or (content_type and "pdf" in content_type):
        return "pdf"
    if s.endswith(".docx") or (content_type and "wordprocessingml" in content_type):
        return "docx"
    if s.endswith((".html", ".htm")) or (content_type and "html" in content_type):
        return "html"
    if s.endswith((".md", ".markdown")):
        return "markdown"
    if s.endswith((".txt", ".log", ".csv", ".json", ".yaml", ".yml", ".toml")):
        return "text"
    return "unknown"


def _parse_pdf(blob: bytes) -> tuple[str, str, int, bool]:
    """Parse a PDF, capping pages and total text to defuse decompression bombs.

    Stops after ``settings.max_pdf_pages`` pages OR once accumulated text passes
    ``settings.max_document_chars``. Returns
    ``(title, text, total_pages, truncated)`` where ``truncated`` is True when
    either cap was hit (so callers can flag the result as incomplete).
    """
    reader = PdfReader(io.BytesIO(blob))
    total_pages = len(reader.pages)
    max_pages = settings.max_pdf_pages
    max_chars = settings.max_document_chars
    parts: list[str] = []
    acc_chars = 0
    truncated = False
    for i, page in enumerate(reader.pages, 1):
        if i > max_pages:
            truncated = True
            break
        try:
            txt = page.extract_text() or ""
        except Exception as e:
            log.warning("pdf page %d failed: %s", i, e)
            continue
        if txt.strip():
            piece = f"## Page {i}\n\n{txt.strip()}"
            parts.append(piece)
            acc_chars += len(piece)
            if acc_chars >= max_chars:
                truncated = True
                break
    title = ""
    try:
        meta = reader.metadata
        if meta and meta.title:
            title = str(meta.title)
    except Exception:
        pass
    return title, "\n\n".join(parts), total_pages, truncated


def _parse_docx(blob: bytes) -> tuple[str, bool]:
    """Parse a docx, capping accumulated text to defuse decompression bombs.

    Stops once accumulated text passes ``settings.max_document_chars``. Returns
    ``(text, truncated)``.
    """
    doc = DocxDocument(io.BytesIO(blob))
    max_chars = settings.max_document_chars
    parts: list[str] = []
    acc_chars = 0
    truncated = False
    for p in doc.paragraphs:
        text = p.text.strip()
        if not text:
            continue
        style = (p.style.name if p.style else "") or ""
        if style.startswith("Heading"):
            level = "".join(c for c in style if c.isdigit()) or "1"
            piece = f"{'#' * int(level)} {text}"
        else:
            piece = text
        parts.append(piece)
        acc_chars += len(piece)
        if acc_chars >= max_chars:
            truncated = True
            break
    if not truncated:
        for table in doc.tables:
            rows = []
            for row in table.rows:
                rows.append(" | ".join(cell.text.strip() for cell in row.cells))
            if rows:
                piece = "\n".join(rows)
                parts.append(piece)
                acc_chars += len(piece)
                if acc_chars >= max_chars:
                    truncated = True
                    break
    return "\n\n".join(parts), truncated


def _parse_html(blob: bytes) -> str:
    html = blob.decode("utf-8", errors="replace")
    return html_to_md(html, heading_style="ATX", bullets="-").strip()


def _parse_text(blob: bytes) -> str:
    return blob.decode("utf-8", errors="replace")


async def _read_remote(url: str) -> tuple[bytes, str | None]:
    # SSRF guard: validate the caller URL before opening a socket.
    assert_url_allowed(url)
    async with httpx.AsyncClient(
        timeout=settings.fetch_timeout,
        # Automatic redirects DISABLED so a 30x cannot jump to an internal IP
        # behind the SSRF guard's back. We follow Location by hand, re-checking
        # each hop with assert_url_allowed.
        follow_redirects=False,
        headers={"User-Agent": settings.user_agent},
        proxy=proxy_url(),
    ) as client:
        current = url
        for _ in range(_MAX_REDIRECTS + 1):
            async with client.stream("GET", current) as resp:
                if resp.status_code in (301, 302, 303, 307, 308):
                    nxt = _resolve_redirect_location(
                        current, resp.headers.get("location")
                    )
                    if not nxt:
                        raise RuntimeError(f"redirect with no Location from {current}")
                    assert_url_allowed(nxt)
                    current = nxt
                    continue
                resp.raise_for_status()
                _check_content_length(resp.headers)
                body = await _accumulate_capped(resp.aiter_bytes())
                return body, resp.headers.get("content-type")
        raise RuntimeError(f"too many redirects (>{_MAX_REDIRECTS}) fetching {url}")


def _slice(full: str, start: int, length: int | None) -> tuple[str, bool, int, int]:
    """Slice [start:start+length] then smart-truncate to settings.max_content_chars.

    Returns ``(sliced_content, truncated, returned_chars, clamped_start)``.

    Invariants the caller depends on:
      * ``returned_chars`` counts SOURCE characters consumed — NOT len(content).
        smart_truncate may append a "[…truncated]" marker that is absent from
        the source, so paginating by ``start + returned_chars`` lands exactly
        on the next un-read source character (no gap, no overlap).
      * ``clamped_start`` is ``start`` clamped into ``[0, len(full)]`` so a
        caller passing a start past EOF sees where the read actually began.
      * ``truncated`` is True only when content was actually withheld: either a
        soft (smart_truncate) cut, or the slice ended before EOF.
    """
    if length is not None and length < 0:
        raise ValueError(f"length must be >= 0, got {length}")

    start = max(0, min(start, len(full)))
    end = len(full) if length is None else min(len(full), start + length)
    chunk = full[start:end]

    truncated_chunk, soft_trunc = smart_truncate(chunk, settings.max_content_chars)
    if soft_trunc:
        # smart_truncate cut the slice AND appended a marker. The real number of
        # SOURCE chars consumed is the pre-marker length, which equals what
        # smart_truncate kept before adding its suffix. Recover it by counting
        # how much of the original `chunk` survived: the kept-prefix length.
        consumed = _source_chars_consumed(chunk)
        returned_chars = consumed
        # End of consumed source for the "did we reach EOF?" decision.
        soft_end = start + consumed
    else:
        returned_chars = len(chunk)
        soft_end = end

    truncated = soft_trunc or soft_end < len(full)
    return truncated_chunk, truncated, returned_chars, start


def _source_chars_consumed(chunk: str) -> int:
    """How many leading SOURCE chars smart_truncate kept (marker excluded).

    Mirrors smart_truncate's boundary logic to recover the pre-marker length,
    so pagination by returned_chars never skips real characters.
    """
    max_chars = settings.max_content_chars
    if len(chunk) <= max_chars:
        return len(chunk)
    head = chunk[:max_chars]
    floor = int(max_chars * 0.7)
    best = -1
    # Same boundary set/logic as formatting.smart_truncate.
    for sep in ("\n\n", "\n", "。", ". ", "！", "! ", "？", "? "):
        idx = head.rfind(sep)
        if idx >= floor and idx + len(sep) > best:
            best = idx + len(sep)
    if best <= 0:
        # Hard cut at max_chars (smart_truncate did head.rstrip() + " …").
        return len(head.rstrip())
    return len(head[:best].rstrip())


def _resolve_local_path(source: str) -> Path:
    """Resolve a local-file source under the opt-in sandbox, or refuse.

    Sandbox policy (chosen by the user):
      * ``file://`` scheme is rejected outright.
      * If ``settings.document_root`` is None, local reads are DISABLED — the
        operator opts in by pointing SEARCH_MCP_DOCUMENT_ROOT at a directory.
      * Otherwise the path is resolved and must stay inside document_root;
        traversal/escape (``../../etc/passwd``, absolute paths outside the
        root, symlink escapes after resolve()) raise.
    """
    parsed = urlparse(source)
    if parsed.scheme == "file":
        raise ValueError(
            "file:// URLs are not allowed for local reads. Pass a plain path "
            "inside SEARCH_MCP_DOCUMENT_ROOT instead."
        )

    root = settings.document_root
    if root is None:
        raise PermissionError(
            "Local file reads are disabled; set SEARCH_MCP_DOCUMENT_ROOT to a "
            "directory to opt in. Remote http(s) sources are unaffected."
        )

    root = Path(root).expanduser().resolve()
    candidate = Path(source).expanduser()
    if not candidate.is_absolute():
        # Relative paths resolve against the sandbox root, not the CWD.
        candidate = root / candidate
    candidate = candidate.resolve()

    if not candidate.is_relative_to(root):
        raise PermissionError(
            f"Refusing to read {source!r}: resolves outside the document_root "
            f"sandbox ({root})."
        )
    if not candidate.exists():
        raise FileNotFoundError(source)
    return candidate


async def read_document(
    source: str,
    *,
    start: int = 0,
    length: int | None = None,
) -> DocumentResult:
    parsed = urlparse(source)
    if parsed.scheme in ("http", "https"):
        blob, ctype = await _read_remote(source)
        fmt = _detect_format(source, ctype)
    else:
        path = _resolve_local_path(source)
        blob = path.read_bytes()
        fmt = _detect_format(str(path))

    title = ""
    pages: int | None = None
    doc_truncated = False
    if fmt == "pdf":
        title, full, pages, doc_truncated = _parse_pdf(blob)
    elif fmt == "docx":
        full, doc_truncated = _parse_docx(blob)
    elif fmt == "html":
        full = _parse_html(blob)
    elif fmt in ("text", "markdown"):
        full = _parse_text(blob)
    else:
        raise ValueError(
            f"Unsupported document format for {source!r}. "
            "Supported: pdf, docx, html, text, markdown."
        )

    content, slice_truncated, returned, clamped_start = _slice(full, start, length)
    # truncated if EITHER the parse hit a bomb-cap OR this slice withheld text.
    return DocumentResult(
        source=source,
        format=fmt,
        title=title,
        content=content,
        truncated=slice_truncated or doc_truncated,
        pages=pages,
        tokens_estimated=estimate_tokens(content),
        total_chars=len(full),
        start=clamped_start,
        returned_chars=returned,
    )
