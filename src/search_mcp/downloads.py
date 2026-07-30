"""Opt-in, ephemeral file downloads.

Writing to a user's disk is the only thing this server does that outlives the
call, so it is off unless someone turns it on:

  * `SEARCH_MCP_DOWNLOAD_DIR` unset (the default) means downloads are
    disabled outright — the same posture as `document_root` for local reads.
  * With no directory configured, the `download` tool ASKS (via MCP
    elicitation) before writing anything, and the answer applies to the
    running process only. Making it permanent means setting the env var.
  * Everything written is ephemeral: files older than
    `SEARCH_MCP_DOWNLOAD_TTL_HOURS` (default 24) are deleted on the next
    download and at startup.

Nothing here trusts a remote filename. Names are sanitized to a single path
component and the final path is re-checked against the download root, because
a Content-Disposition header is attacker-controlled input.
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

# Set by `enable_for_session` when the user accepts the elicitation. Process-
# scoped on purpose: an interactive "yes" should not silently persist into
# every future run of the server.
_session_dir: Path | None = None

# Anything outside this set is replaced. Deliberately strict: the remote
# controls this string, and it becomes a filename.
_UNSAFE = re.compile(r"[^A-Za-z0-9._-]+")
_MAX_NAME = 80


def default_download_dir() -> Path:
    """Where downloads land when the user opts in without naming a directory."""
    return Path(settings.cache_dir) / "downloads"


def download_dir() -> Path | None:
    """The active download directory, or None when downloads are disabled."""
    if settings.download_dir is not None:
        return Path(settings.download_dir).expanduser()
    return _session_dir


def is_enabled() -> bool:
    return download_dir() is not None


def enable_for_session(path: Path | None = None) -> Path:
    """Turn downloads on for this process only. Returns the directory."""
    global _session_dir
    _session_dir = Path(path or default_download_dir()).expanduser()
    _session_dir.mkdir(parents=True, exist_ok=True)
    return _session_dir


def disable_for_session() -> None:
    """Undo `enable_for_session` (used by tests and by an explicit opt-out)."""
    global _session_dir
    _session_dir = None


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
    if len(name) > _MAX_NAME:
        stem, dot, ext = name.rpartition(".")
        name = (stem[: _MAX_NAME - len(ext) - 1] + dot + ext) if dot else name[:_MAX_NAME]
    if "." not in name and media_type:
        ext = _extension_for(media_type)
        if ext:
            name = f"{name}.{ext}"
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
    if root is None or not root.exists():
        return 0
    ttl_seconds = max(0, settings.download_ttl_hours) * 3600
    if ttl_seconds == 0:
        return 0
    cutoff = (now if now is not None else time.time()) - ttl_seconds
    removed = 0
    for path in root.iterdir():
        if not path.is_file():
            continue
        try:
            if path.stat().st_mtime < cutoff:
                path.unlink()
                removed += 1
        except OSError as exc:  # a file vanishing under us is not an error
            log.debug("could not purge %s: %s", path, exc)
    if removed:
        log.info("purged %d expired download(s) from %s", removed, root)
    return removed


def save(url: str, blob: bytes, media_type: str = "") -> Path:
    """Write `blob` into the download directory and return its path.

    Raises PermissionError when downloads are disabled, and ValueError when
    the payload exceeds `SEARCH_MCP_DOWNLOAD_MAX_MB`.
    """
    root = download_dir()
    if root is None:
        raise PermissionError(
            "Downloads are disabled. Set SEARCH_MCP_DOWNLOAD_DIR to a directory "
            "to enable them permanently, or call the `download` tool, which will "
            "ask before writing anything."
        )
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
