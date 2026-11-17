"""Default-on, ephemeral file downloads.

Files are saved under the cache directory unless an operator overrides the
sandbox with `SEARCH_MCP_DOWNLOAD_DIR` or disables downloads with
`SEARCH_MCP_DOWNLOAD_ENABLED=false`. Files older than
`SEARCH_MCP_DOWNLOAD_TTL_HOURS` (default 24) are deleted on the next download
and at startup.

Nothing here trusts a remote filename. Names are sanitized to a single path
component and the final path is re-checked against the download root.
"""

from __future__ import annotations

import hashlib
import logging
import re
import time
from pathlib import Path
from urllib.parse import unquote, urlparse

from .config import settings

log = logging.getLogger(__name__)

_DISABLED_MESSAGE = (
    "Downloads are disabled by SEARCH_MCP_DOWNLOAD_ENABLED=false. "
    "Remove that setting or set it to true; SEARCH_MCP_DOWNLOAD_DIR "
    "only overrides the destination."
)

# Anything outside this set is replaced. Deliberately strict: the remote
# controls this string, and it becomes a filename.
_UNSAFE = re.compile(r"[^A-Za-z0-9._-]+")
_MAX_NAME = 80


def default_download_dir() -> Path:
    """The default download directory derived from the active cache root."""
    return settings.cache_dir / "downloads"


def download_dir() -> Path | None:
    """The active download directory, or None when explicitly disabled."""
    if not settings.download_enabled:
        return None
    if settings.download_dir is not None:
        return settings.download_dir
    return default_download_dir()


def require_download_dir() -> Path:
    """Return the active download directory or reject operator disablement."""
    root = download_dir()
    if root is None:
        raise PermissionError(_DISABLED_MESSAGE)
    return root


def safe_filename(url: str, media_type: str = "", blob: bytes = b"") -> str:
    """Derive a safe, collision-resistant filename for a downloaded URL.

    The remote's own name is only ever a *hint*: it is stripped to one path
    component, filtered to a conservative character set, length-capped, and
    prefixed with a content hash so two different resources that claim the
    same name cannot overwrite each other.
    """
    raw = unquote(urlparse(url).path).rsplit("/", 1)[-1]
    # Strip any directory traversal that survived unquoting.
    raw = raw.replace("\\", "/").rsplit("/", 1)[-1]
    name = _UNSAFE.sub("_", raw).strip("._-")
    if not name:
        name = "download"
    if "." not in name and media_type:
        ext = _extension_for(media_type)
        if ext:
            name = f"{name}.{ext}"
    # Apply the cap after MIME inference so every final component (including
    # the eight-character digest and dash below) is at most _MAX_NAME + 9.
    if len(name) > _MAX_NAME:
        stem, dot, ext = name.rpartition(".")
        ext = ext[:16]
        name = (stem[: _MAX_NAME - len(ext) - 1] + dot + ext) if dot else name[:_MAX_NAME]
    digest = hashlib.sha256(blob or url.encode()).hexdigest()[:8]
    return f"{digest}-{name}"


def _extension_for(media_type: str) -> str:
    import mimetypes

    guessed = mimetypes.guess_extension(media_type.split(";", 1)[0].strip())
    return guessed.lstrip(".") if guessed else ""


def purge_expired(now: float | None = None) -> int:
    """Delete downloads older than the TTL. Returns how many were removed.

    Called before each download and at startup rather than on a timer: the
    server is often a short-lived subprocess, so a background sweeper would
    frequently never run.
    """
    root = download_dir()
    if root is None:
        return 0
    ttl_seconds = max(0, settings.download_ttl_hours) * 3600
    if ttl_seconds == 0:
        return 0
    cutoff = (now if now is not None else time.time()) - ttl_seconds
    removed = 0
    try:
        for path in root.iterdir():
            try:
                if path.is_file() and path.stat().st_mtime < cutoff:
                    path.unlink()
                    removed += 1
            except OSError as exc:  # a file vanishing under us is not an error
                log.debug("could not purge %s: %s", path, exc)
    except OSError as exc:
        log.debug("could not scan download directory %s: %s", root, exc)
        return removed
    if removed:
        log.info("purged %d expired download(s) from %s", removed, root)
    return removed


def save(url: str, blob: bytes, media_type: str = "") -> Path:
    """Write `blob` into the download directory and return its path.

    Raises PermissionError when downloads are disabled, and ValueError when
    the payload exceeds `SEARCH_MCP_DOWNLOAD_MAX_MB`.
    """
    root = require_download_dir()
    cap = settings.download_max_mb * 1024 * 1024
    if cap and len(blob) > cap:
        raise ValueError(
            f"Refusing to save {len(blob) / 1024 / 1024:.1f} MB: over the "
            f"{settings.download_max_mb} MB SEARCH_MCP_DOWNLOAD_MAX_MB limit."
        )

    root.mkdir(parents=True, exist_ok=True)
    resolved_root = root.resolve()
    target = (resolved_root / safe_filename(url, media_type, blob)).resolve()
    # Belt and braces: safe_filename already collapses the name to a single
    # component, but the final path is re-checked so no future change to the
    # naming rules can turn into a write outside the sandbox.
    if not target.is_relative_to(resolved_root):
        raise PermissionError(f"Refusing to write outside {resolved_root}")

    target.write_bytes(blob)
    return target
