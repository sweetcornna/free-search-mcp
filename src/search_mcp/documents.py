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
from .formatting import estimate_tokens, smart_truncate

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


def _parse_pdf(blob: bytes) -> tuple[str, str, int]:
    reader = PdfReader(io.BytesIO(blob))
    parts: list[str] = []
    for i, page in enumerate(reader.pages, 1):
        try:
            txt = page.extract_text() or ""
        except Exception as e:
            log.warning("pdf page %d failed: %s", i, e)
            continue
        if txt.strip():
            parts.append(f"## Page {i}\n\n{txt.strip()}")
    title = ""
    try:
        meta = reader.metadata
        if meta and meta.title:
            title = str(meta.title)
    except Exception:
        pass
    return title, "\n\n".join(parts), len(reader.pages)


def _parse_docx(blob: bytes) -> str:
    doc = DocxDocument(io.BytesIO(blob))
    parts: list[str] = []
    for p in doc.paragraphs:
        text = p.text.strip()
        if not text:
            continue
        style = (p.style.name if p.style else "") or ""
        if style.startswith("Heading"):
            level = "".join(c for c in style if c.isdigit()) or "1"
            parts.append(f"{'#' * int(level)} {text}")
        else:
            parts.append(text)
    for table in doc.tables:
        rows = []
        for row in table.rows:
            rows.append(" | ".join(cell.text.strip() for cell in row.cells))
        if rows:
            parts.append("\n".join(rows))
    return "\n\n".join(parts)


def _parse_html(blob: bytes) -> str:
    html = blob.decode("utf-8", errors="replace")
    return html_to_md(html, heading_style="ATX", bullets="-").strip()


def _parse_text(blob: bytes) -> str:
    return blob.decode("utf-8", errors="replace")


async def _read_remote(url: str) -> tuple[bytes, str | None]:
    async with httpx.AsyncClient(
        timeout=settings.fetch_timeout,
        follow_redirects=True,
        headers={"User-Agent": settings.user_agent},
    ) as client:
        resp = await client.get(url)
        resp.raise_for_status()
        return resp.content, resp.headers.get("content-type")


def _slice(full: str, start: int, length: int | None) -> tuple[str, bool, int]:
    """Slice [start:start+length] then smart-truncate to settings.max_content_chars.

    Returns (sliced_content, truncated, returned_chars).
    """
    start = max(0, min(start, len(full)))
    end = len(full) if length is None else min(len(full), start + length)
    chunk = full[start:end]
    chunk, soft_trunc = smart_truncate(chunk, settings.max_content_chars)
    truncated = soft_trunc or end < len(full)
    return chunk, truncated, len(chunk)


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
        path = Path(source).expanduser().resolve()
        if not path.exists():
            raise FileNotFoundError(source)
        blob = path.read_bytes()
        fmt = _detect_format(str(path))

    title = ""
    pages: int | None = None
    if fmt == "pdf":
        title, full, pages = _parse_pdf(blob)
    elif fmt == "docx":
        full = _parse_docx(blob)
    elif fmt == "html":
        full = _parse_html(blob)
    elif fmt in ("text", "markdown"):
        full = _parse_text(blob)
    else:
        raise ValueError(
            f"Unsupported document format for {source!r}. "
            "Supported: pdf, docx, html, text, markdown."
        )

    content, truncated, returned = _slice(full, start, length)
    return DocumentResult(
        source=source,
        format=fmt,
        title=title,
        content=content,
        truncated=truncated,
        pages=pages,
        tokens_estimated=estimate_tokens(content),
        total_chars=len(full),
        start=start,
        returned_chars=returned,
    )
