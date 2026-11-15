"""Which on-disk profile this process's Chromium uses, and keeping it small.

Every MCP client starts a server of its own, and they all used to point
Chromium at the one ``<cache_dir>/browser_profile``. Seen 2026-11-15 on a
desktop with several Claude Code and Codex sessions open: two dozen servers,
five headless Chromiums writing that directory at the same moment, and 1.5 GB
on disk, most of it HTTP, V8 and GPU caches that nothing ever pruned.

A process now leases a profile. The shared one goes to whichever process holds
the OS lock on ``browser_profile.lock``, so a killed process frees it without
cleanup. Every other process gets a throwaway profile under
``browser_profiles/``, deleted when that process shuts down or, if it was
killed, by the next process that finds its lock free. Caches are pruned when a
lease is taken, which is safe because the lock means no other Chromium has the
directory open. Cookies, local storage and IndexedDB are kept.

Cookies from an interactive login reach every process through
``browser_login_cookies.json``: the login window runs in a throwaway profile of
its own, so it never waits on, or writes under, a server that holds the shared
one.
"""

from __future__ import annotations

import json
import logging
import os
import secrets
import shutil
from contextlib import suppress
from pathlib import Path

log = logging.getLogger(__name__)

SHARED_NAME = "browser_profile"
PRIVATE_DIR = "browser_profiles"
LOGIN_COOKIES = "browser_login_cookies.json"

# Regenerable data at the top of a user-data-dir. The model stores and CRX
# caches are what the component updater of a real Chrome downloads.
_TOP_LEVEL_CACHES = (
    "BrowserMetrics",
    "BrowserMetrics-spare.pma",
    "component_crx_cache",
    "extensions_crx_cache",
    "GPUPersistentCache",
    "GraphiteDawnCache",
    "GrShaderCache",
    "OnDeviceHeadSuggestModel",
    "optimization_guide_model_store",
    "OptGuideOnDeviceModel",
    "ShaderCache",
    "WasmTtsEngine",
)

# Regenerable data inside each profile (``Default``, ``Profile 1``, ...).
_PROFILE_CACHES = (
    "Cache",
    "Code Cache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "GPUCache",
    "Service Worker",
    "Shared Dictionary",
)


def _lock(fd: int) -> bool:
    """Take an exclusive, non-blocking lock on ``fd``. False if it is held."""
    try:
        if os.name == "nt":
            import msvcrt

            os.lseek(fd, 0, os.SEEK_SET)
            msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
        else:
            import fcntl

            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        return False
    return True


def _unlock_close(fd: int) -> None:
    if os.name == "nt":
        with suppress(OSError):
            import msvcrt

            os.lseek(fd, 0, os.SEEK_SET)
            msvcrt.locking(fd, msvcrt.LK_UNLCK, 1)
    with suppress(OSError):
        os.close(fd)


def _same_file(fd: int, path: Path) -> bool:
    """True when ``path`` still names the file ``fd`` has open.

    A sweeper unlinks a lock file while it holds the lock, so a lock taken on
    a file that has since been unlinked guards nothing.
    """
    try:
        return os.fstat(fd).st_ino == os.stat(path).st_ino
    except OSError:
        return False


def _remove(path: Path) -> None:
    if path.is_dir() and not path.is_symlink():
        shutil.rmtree(path, ignore_errors=True)
    else:
        with suppress(OSError):
            path.unlink()


def prune_caches(user_data_dir: Path) -> None:
    """Delete the regenerable caches in a user-data-dir no browser has open."""
    if not user_data_dir.is_dir():
        return
    for name in _TOP_LEVEL_CACHES:
        _remove(user_data_dir / name)
    for profile in user_data_dir.iterdir():
        if profile.is_dir() and (profile / "Preferences").exists():
            for name in _PROFILE_CACHES:
                _remove(profile / name)


class ProfileLease:
    """A user-data-dir this process may launch Chromium on, until released."""

    def __init__(
        self, path: Path, *, shared: bool, fd: int, lock_path: Path | None = None
    ) -> None:
        self.path = path
        self.shared = shared
        self._fd: int | None = fd
        self._lock_path = lock_path

    def release(self) -> None:
        """Give the profile up. Call only after its browser has exited.

        A throwaway profile is deleted. The shared one stays on disk, and its
        lock becomes free for the next process.
        """
        fd, self._fd = self._fd, None
        if fd is None:
            return
        if self.shared:
            _unlock_close(fd)
            return
        shutil.rmtree(self.path, ignore_errors=True)
        if os.name == "nt":
            # Windows cannot unlink a file that is open.
            _unlock_close(fd)
            with suppress(OSError):
                self._lock_path.unlink()
        else:
            with suppress(OSError):
                self._lock_path.unlink()
            _unlock_close(fd)


def sweep_private(cache_dir: Path) -> None:
    """Delete throwaway profiles whose process is gone (its lock is free)."""
    root = cache_dir / PRIVATE_DIR
    if not root.is_dir():
        return
    for lock_path in root.glob("*.lock"):
        try:
            fd = os.open(lock_path, os.O_RDWR)
        except OSError:
            continue
        if not _lock(fd):
            _unlock_close(fd)
            continue
        shutil.rmtree(root / lock_path.stem, ignore_errors=True)
        if os.name == "nt":
            _unlock_close(fd)
            with suppress(OSError):
                lock_path.unlink()
        else:
            # Unlinked under the lock, so a process that opened this file
            # before us finds it gone once it gets the lock, and picks
            # another name.
            with suppress(OSError):
                lock_path.unlink()
            _unlock_close(fd)


def acquire_private(cache_dir: Path) -> ProfileLease:
    """A new, empty throwaway profile, locked to this process."""
    root = cache_dir / PRIVATE_DIR
    root.mkdir(parents=True, exist_ok=True)
    sweep_private(cache_dir)
    for _ in range(8):
        name = f"{os.getpid()}-{secrets.token_hex(4)}"
        lock_path = root / f"{name}.lock"
        try:
            fd = os.open(lock_path, os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
        except FileExistsError:
            continue
        if not _lock(fd) or not _same_file(fd, lock_path):
            _unlock_close(fd)
            continue
        path = root / name
        path.mkdir(exist_ok=True)
        return ProfileLease(path, shared=False, fd=fd, lock_path=lock_path)
    raise OSError(f"could not create a browser profile under {root}")


def acquire(cache_dir: Path) -> ProfileLease:
    """The shared profile if no other process holds it, else a throwaway one.

    Either way the caches of the leased profile are pruned first.
    """
    cache_dir.mkdir(parents=True, exist_ok=True)
    shared = cache_dir / SHARED_NAME
    fd = os.open(cache_dir / f"{SHARED_NAME}.lock", os.O_CREAT | os.O_RDWR, 0o600)
    if _lock(fd):
        prune_caches(shared)
        sweep_private(cache_dir)
        return ProfileLease(shared, shared=True, fd=fd)
    _unlock_close(fd)
    log.info("browser profile in use by another server; using a throwaway one")
    return acquire_private(cache_dir)


def _cookie_key(cookie: dict) -> tuple:
    return (cookie.get("name"), cookie.get("domain"), cookie.get("path"))


def read_login_cookies(cache_dir: Path) -> list[dict]:
    """The cookies saved by the last interactive login, or ``[]``."""
    try:
        data = json.loads((cache_dir / LOGIN_COOKIES).read_text("utf-8"))
    except (OSError, ValueError):
        return []
    return [c for c in data if isinstance(c, dict)] if isinstance(data, list) else []


def write_login_cookies(cache_dir: Path, base: list[dict], cookies: list[dict]) -> None:
    """Save ``cookies`` over ``base``, a cookie with the same name, domain and
    path replacing the old one. Written atomically and readable only by the
    user, like the profile's own cookie store."""
    merged = {_cookie_key(c): c for c in base}
    merged.update((_cookie_key(c), c) for c in cookies)
    path = cache_dir / LOGIN_COOKIES
    tmp = path.with_name(f"{path.name}.{os.getpid()}.tmp")
    fd = os.open(tmp, os.O_CREAT | os.O_TRUNC | os.O_WRONLY, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as fh:
        json.dump(list(merged.values()), fh)
    os.replace(tmp, path)
