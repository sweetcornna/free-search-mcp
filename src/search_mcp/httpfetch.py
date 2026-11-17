"""Shared SSRF-guarded HTTP GET plumbing.

ONE redirect-following, size-capped GET loop instead of the three copies that
used to live in fetcher/documents/structured. Two client flavors on purpose:

* :func:`curl_stream_capped` — curl_cffi with a Chrome fingerprint, for the
  page-fetch path where bot shields inspect JA3/H2.
* :func:`httpx_stream_capped` — plain httpx, for documents/structured where
  the content isn't bot-shielded and httpx's streaming API is enough.

Contract shared by both: the CALLER validates the initial URL (so the SSRF
guard runs before a client is even constructed) and constructs the client in
its own module (so tests can monkeypatch ``fetcher.AsyncSession`` /
``httpx.AsyncClient`` exactly as before); every REDIRECT hop is re-validated
here, so a 30x can never jump to an internal address. Automatic redirects
must be disabled on the client.
"""
from __future__ import annotations

import re
import zlib
from typing import TYPE_CHECKING, Any
from urllib.parse import urljoin

from .config import settings
from .net import curl_proxy_kwargs
from .url_safety import assert_url_allowed_async

if TYPE_CHECKING:
    import httpx

# Match the engine fast-path: real Chrome JA3/JA4 + H2 fingerprint so target
# sites don't see "headless client claiming to be Chrome".
IMPERSONATE = "chrome131"

# Cap on manually-followed redirect hops. We disable the HTTP client's
# automatic redirect handling (which would chase a 30x straight to an
# internal IP, bypassing the SSRF guard) and follow Location headers by hand,
# re-validating each hop before connecting.
_MAX_REDIRECTS = 5

_REDIRECT_STATUSES = (301, 302, 303, 307, 308)


class MaxBytesExceededError(RuntimeError):
    """Raised when a response body grows past settings.max_response_bytes."""


class FetchError(RuntimeError):
    """A fetch that failed for a reason worth telling the caller about.

    These used to be bare ``RuntimeError``s, which was fine while the MCP SDK
    forwarded every exception message to the client. Since SDK 2.1 only an
    *anticipated* failure does; anything else reaches the model as a bare
    "Error executing tool fetch". server.py's tool boundary cannot allow-list
    ``RuntimeError`` wholesale — that would forward the text of genuine bugs
    too — so the failures we raise on purpose get a type of their own. It still
    subclasses RuntimeError, so every existing ``except RuntimeError`` and
    ``pytest.raises(RuntimeError)`` keeps matching.
    """


def curl_session_kwargs() -> dict[str, Any]:
    """Constructor kwargs for the curl_cffi AsyncSession used by the fetch path.

    No explicit User-Agent: curl_cffi sets one matching the impersonated
    Chrome build, keeping the UA <-> JA3/H2 fingerprints consistent.
    """
    return {
        "impersonate": IMPERSONATE,
        "timeout": settings.fetch_timeout,
        # Automatic redirects are DISABLED: a 30x could otherwise jump straight
        # to an internal IP, bypassing the per-hop SSRF check in the loop.
        "allow_redirects": False,
        "headers": {
            "Accept-Language": settings.accept_language,
            "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        },
        **curl_proxy_kwargs(),
    }


def httpx_client_kwargs() -> dict[str, Any]:
    """Constructor kwargs for the httpx.AsyncClient used by documents/structured."""
    from .net import proxy_url

    return {
        "timeout": settings.fetch_timeout,
        # Automatic redirects DISABLED — see curl_session_kwargs.
        "follow_redirects": False,
        "headers": {"User-Agent": settings.user_agent, "Accept-Encoding": "gzip, deflate"},
        "proxy": proxy_url(),
    }


def _check_content_length(headers: Any) -> None:
    """Reject up front if the declared Content-Length exceeds the cap.

    A streaming guard (_accumulate_capped) still backstops servers that lie
    or omit the header.
    """
    raw = headers.get("content-length") or headers.get("Content-Length")
    if not raw:
        return
    try:
        declared = int(raw)
    except (TypeError, ValueError):
        return
    cap = settings.max_response_bytes
    if declared > cap:
        raise MaxBytesExceededError(
            f"Response Content-Length {declared} exceeds cap {cap} bytes; refusing to download."
        )


async def _accumulate_capped(aiter: Any) -> bytes:
    """Buffer an async byte-chunk iterator, aborting once it passes the cap.

    The cap is settings.max_response_bytes, so an oversized (or
    Content-Length-lying) body never fully buffers into memory.
    """
    cap = settings.max_response_bytes
    buf = bytearray()
    async for chunk in aiter:
        if not chunk:
            continue
        if len(chunk) > cap - len(buf):
            raise MaxBytesExceededError(
                f"Response body exceeded cap {cap} bytes while streaming; aborted."
            )
        buf.extend(chunk)
    return bytes(buf)


async def _inflate_capped(aiter: Any, encoding: str) -> bytes:
    """Decode raw gzip/deflate with independent wire and output byte budgets.

    httpx's aiter_bytes decodes a whole input chunk before yielding, so its
    chunk_size cannot prevent expansion bombs. zlib's max_length bounds each
    allocation to the remaining output budget plus one detection byte. Never
    call flush(): its length argument is an initial buffer size, not a cap.
    """
    cap = settings.max_response_bytes
    buf = bytearray()
    wire_size = 0
    decoder = zlib.decompressobj(16 + zlib.MAX_WBITS) if encoding == "gzip" else None
    prefix = b""
    async for chunk in aiter:
        wire_size += len(chunk)
        if wire_size > cap:
            raise MaxBytesExceededError(
                f"Compressed response exceeded wire cap {cap} bytes while streaming; aborted."
            )
        if not chunk:
            continue
        if decoder is None:
            # HTTP deflate is normally zlib-wrapped, but some servers send raw
            # DEFLATE. Inspect the two-byte header, even if split across reads,
            # before choosing a decoder, rather than replaying an entire body.
            chunk = prefix + chunk
            if len(chunk) < 2:
                prefix = chunk
                continue
            wrapped = (
                chunk[0] & 0x0F == 8
                and chunk[0] >> 4 <= 7
                and int.from_bytes(chunk[:2], "big") % 31 == 0
            )
            decoder = zlib.decompressobj(zlib.MAX_WBITS if wrapped else -zlib.MAX_WBITS)
        while chunk:
            if decoder.eof:
                if encoding != "gzip":
                    raise FetchError("Unexpected data after deflate response")
                # Gzip permits concatenated members; they share one budget.
                decoder = zlib.decompressobj(16 + zlib.MAX_WBITS)
            remaining = cap - len(buf)
            try:
                decoded = decoder.decompress(chunk, max_length=remaining + 1)
            except zlib.error as exc:
                raise FetchError(f"Invalid {encoding} response: {exc}") from exc
            if len(decoded) > remaining:
                raise MaxBytesExceededError(
                    f"Decoded response exceeded cap {cap} bytes while streaming; aborted."
                )
            buf.extend(decoded)
            chunk = decoder.unused_data if decoder.eof else decoder.unconsumed_tail
    if decoder is None or not decoder.eof:
        raise FetchError(f"Truncated {encoding} response")
    return bytes(buf)


def _resolve_redirect_location(base_url: str, location: str | None) -> str | None:
    """Resolve a (possibly relative) Location against base_url. None if absent."""
    if not location:
        return None
    return urljoin(base_url, location)


# Charset detection for the HTTP text path. We can't blindly decode as UTF-8:
# many CJK pages (baidu/zhihu hits served as GBK/GB2312/Big5, Japanese pages as
# Shift-JIS/EUC-JP) declare their charset in the Content-Type header or an HTML
# <meta> tag, and decoding those as UTF-8 yields mojibake the LLM can't read.
_CTYPE_CHARSET_RE = re.compile(r"charset\s*=\s*[\"']?([\w\-]+)", re.I)
_META_CHARSET_RE = re.compile(
    rb"""<meta[^>]+charset\s*=\s*["']?\s*([a-zA-Z0-9_\-]+)""", re.I
)


def _charset_from_ctype(ctype: str) -> str | None:
    m = _CTYPE_CHARSET_RE.search(ctype or "")
    return m.group(1).lower() if m else None


def _sniff_meta_charset(body: bytes) -> str | None:
    """Best-effort: read the charset from an HTML <meta> tag in the head."""
    m = _META_CHARSET_RE.search(body[:4096])
    if not m:
        return None
    try:
        return m.group(1).decode("ascii").lower()
    except (UnicodeDecodeError, AttributeError):
        return None


def _decode_body(body: bytes, ctype: str) -> str:
    """Decode a response body using the declared/sniffed charset, not blind UTF-8.

    Precedence: Content-Type header charset > HTML <meta> charset > UTF-8.
    Unknown/invalid codecs fall back to UTF-8 so we never raise on decode.
    """
    enc = _charset_from_ctype(ctype)
    if not enc and (not ctype or "html" in ctype or "xml" in ctype):
        enc = _sniff_meta_charset(body)
    enc = enc or "utf-8"
    try:
        return body.decode(enc, errors="replace")
    except (LookupError, UnicodeError):
        # Unknown labels and codecs that reject errors='replace' (e.g. IDNA).
        return body.decode("utf-8", errors="replace")


async def _close_curl_stream(resp: Any) -> None:
    """End a curl_cffi stream, abandoning whatever of the body is unread.

    ``aclose()`` alone waits for the transfer to finish, so a body over the
    cap or behind an error status used to download in full first. Setting
    ``quit_now`` makes curl abort at the next chunk it receives.
    """
    quit_now = getattr(resp, "quit_now", None)
    if quit_now is not None:
        quit_now.set()
    await resp.aclose()


async def curl_stream_capped(client: Any, url: str) -> tuple[str, str]:
    """The curl_cffi GET loop: manual redirects, per-hop SSRF check, size caps.

    Returns ``(content_type, decoded_text)``. Raises on non-2xx terminal
    responses. The caller has already validated ``url`` itself.
    """
    current = url
    for _ in range(_MAX_REDIRECTS + 1):
        resp = await client.get(current, stream=True)
        if resp.status_code in _REDIRECT_STATUSES:
            # Close the redirect response without buffering its body.
            await _close_curl_stream(resp)
            nxt = _resolve_redirect_location(current, resp.headers.get("location"))
            if not nxt:
                raise FetchError(f"redirect with no Location from {current}")
            await assert_url_allowed_async(nxt)  # re-validate EACH hop
            current = nxt
            continue
        # Terminal response: enforce caps, then stream the body.
        try:
            resp.raise_for_status()
            _check_content_length(resp.headers)
            body = await _accumulate_capped(resp.aiter_content())
        finally:
            await _close_curl_stream(resp)
        ctype = resp.headers.get("content-type", "")
        return ctype, _decode_body(body, ctype)
    raise FetchError(f"too many redirects (>{_MAX_REDIRECTS}) fetching {url}")


async def httpx_stream_capped(
    client: httpx.AsyncClient, url: str, *, raise_for_status: bool
) -> tuple[int, str, bytes]:
    """The httpx GET loop: manual redirects, per-hop SSRF check, size caps.

    Returns ``(status, content_type, body)``. ``raise_for_status=False`` keeps
    a 403/503 bot-block shell (structured's meta_fallback path) instead of
    raising. The caller has already validated ``url`` itself.
    """
    current = url
    for _ in range(_MAX_REDIRECTS + 1):
        async with client.stream("GET", current) as resp:
            if resp.status_code in _REDIRECT_STATUSES:
                nxt = _resolve_redirect_location(current, resp.headers.get("location"))
                if not nxt:
                    raise FetchError(f"redirect with no Location from {current}")
                await assert_url_allowed_async(nxt)  # re-validate EACH hop
                current = nxt
                continue
            if raise_for_status:
                resp.raise_for_status()
            _check_content_length(resp.headers)
            encoding = resp.headers.get("content-encoding", "identity").strip().lower()
            if encoding in ("", "identity"):
                # No content decoder can expand this branch.
                body = await _accumulate_capped(resp.aiter_bytes())
            elif encoding in ("gzip", "deflate"):
                body = await _inflate_capped(resp.aiter_raw(), encoding)
            else:
                # Includes stacked encodings and codecs without a bounded
                # incremental API; never hand these to httpx's auto-decoder.
                raise FetchError(f"Unsupported compressed Content-Encoding: {encoding}")
            return resp.status_code, resp.headers.get("content-type", "") or "", body
    raise FetchError(f"too many redirects (>{_MAX_REDIRECTS}) fetching {url}")
