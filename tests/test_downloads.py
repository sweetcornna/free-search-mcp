"""Opt-in ephemeral downloads.

Three properties matter here and each has teeth:
  1. Nothing is written unless the user allowed it.
  2. A remote-controlled filename cannot escape the download directory.
  3. What is written goes away on its own.
"""

from __future__ import annotations

import time
from pathlib import Path

import pytest

from search_mcp import downloads
from search_mcp.config import settings

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.


@pytest.fixture(autouse=True)
def _reset_session_state():
    """Downloads are process-scoped state; never leak them between tests."""
    downloads.disable_for_session()
    yield
    downloads.disable_for_session()


@pytest.fixture
def enabled(tmp_path, monkeypatch):
    monkeypatch.setattr(settings, "download_dir", None)
    return downloads.enable_for_session(tmp_path / "dl")


# ---------------------------------------------------------------------------
# Off by default
# ---------------------------------------------------------------------------


def test_disabled_by_default(monkeypatch):
    monkeypatch.setattr(settings, "download_dir", None)
    assert downloads.is_enabled() is False
    assert downloads.download_dir() is None


def test_saving_while_disabled_raises_instead_of_writing(monkeypatch):
    monkeypatch.setattr(settings, "download_dir", None)
    with pytest.raises(PermissionError, match="disabled"):
        downloads.save("https://x.example/a.bin", b"data")


def test_env_setting_enables_without_asking(monkeypatch, tmp_path):
    """A configured directory is a standing answer; no prompt needed."""
    monkeypatch.setattr(settings, "download_dir", tmp_path / "configured")
    assert downloads.is_enabled() is True
    assert downloads.download_dir() == tmp_path / "configured"


def test_session_opt_in_does_not_outlive_the_process(tmp_path, monkeypatch):
    monkeypatch.setattr(settings, "download_dir", None)
    downloads.enable_for_session(tmp_path / "dl")
    assert downloads.is_enabled() is True
    downloads.disable_for_session()
    assert downloads.is_enabled() is False


# ---------------------------------------------------------------------------
# Filename safety — the remote controls this string
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "url",
    [
        "https://x.example/../../etc/passwd",
        "https://x.example/%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        "https://x.example/a/b/../../../../root/.ssh/id_rsa",
        "https://x.example/..%5c..%5cwindows%5csystem32",
    ],
)
def test_traversal_attempts_collapse_to_one_component(url):
    name = downloads.safe_filename(url)
    assert "/" not in name
    assert "\\" not in name
    assert ".." not in name


def test_saved_path_stays_inside_the_download_dir(enabled: Path):
    path = downloads.save("https://x.example/../../escape.txt", b"x")
    assert path.parent == enabled.resolve()


def test_filename_is_content_addressed_so_collisions_cannot_overwrite(enabled: Path):
    first = downloads.save("https://a.example/report.pdf", b"one")
    second = downloads.save("https://b.example/report.pdf", b"two")
    assert first != second
    assert first.read_bytes() == b"one"
    assert second.read_bytes() == b"two"


def test_identical_bytes_reuse_one_file(enabled: Path):
    a = downloads.save("https://a.example/x.bin", b"same")
    b = downloads.save("https://a.example/x.bin", b"same")
    assert a == b


def test_long_names_are_capped(enabled: Path):
    path = downloads.save(f"https://x.example/{'n' * 500}.bin", b"x")
    assert len(path.name) < 120


def test_extension_is_inferred_from_media_type_when_missing():
    name = downloads.safe_filename("https://x.example/download", "image/png", b"x")
    assert name.endswith(".png")


def test_nameless_url_still_produces_a_file(enabled: Path):
    path = downloads.save("https://x.example/", b"x")
    assert path.is_file()


# ---------------------------------------------------------------------------
# Size cap
# ---------------------------------------------------------------------------


def test_oversized_download_is_refused(enabled: Path, monkeypatch):
    monkeypatch.setattr(settings, "download_max_mb", 1)
    with pytest.raises(ValueError, match="over the 1 MB"):
        downloads.save("https://x.example/big.bin", b"x" * (2 * 1024 * 1024))


def test_nothing_is_written_when_the_size_cap_trips(enabled: Path, monkeypatch):
    monkeypatch.setattr(settings, "download_max_mb", 1)
    with pytest.raises(ValueError):
        downloads.save("https://x.example/big.bin", b"x" * (2 * 1024 * 1024))
    assert list(enabled.iterdir()) == []


# ---------------------------------------------------------------------------
# Expiry
# ---------------------------------------------------------------------------


def test_expired_files_are_purged(enabled: Path, monkeypatch):
    monkeypatch.setattr(settings, "download_ttl_hours", 24)
    path = downloads.save("https://x.example/old.bin", b"x")
    # Backdate past the TTL.
    old = time.time() - 25 * 3600
    import os

    os.utime(path, (old, old))

    assert downloads.purge_expired() == 1
    assert not path.exists()


def test_fresh_files_survive_the_purge(enabled: Path, monkeypatch):
    monkeypatch.setattr(settings, "download_ttl_hours", 24)
    path = downloads.save("https://x.example/new.bin", b"x")
    assert downloads.purge_expired() == 0
    assert path.exists()


def test_ttl_zero_disables_expiry(enabled: Path, monkeypatch):
    """0 means "keep forever" — it must not mean "delete everything"."""
    monkeypatch.setattr(settings, "download_ttl_hours", 0)
    path = downloads.save("https://x.example/keep.bin", b"x")
    import os

    old = time.time() - 999 * 3600
    os.utime(path, (old, old))
    assert downloads.purge_expired() == 0
    assert path.exists()


def test_purge_is_a_noop_when_downloads_are_disabled(monkeypatch):
    monkeypatch.setattr(settings, "download_dir", None)
    assert downloads.purge_expired() == 0


def test_purge_ignores_subdirectories(enabled: Path, monkeypatch):
    monkeypatch.setattr(settings, "download_ttl_hours", 1)
    sub = enabled / "nested"
    sub.mkdir()
    import os

    old = time.time() - 999 * 3600
    os.utime(sub, (old, old))
    assert downloads.purge_expired() == 0
    assert sub.exists()


# ---------------------------------------------------------------------------
# The tool asks before writing
# ---------------------------------------------------------------------------


class _Ctx:
    """Stand-in for the MCP Context, recording what the user was asked."""

    def __init__(self, action="accept", enable=True, raises=False):
        self.action, self.enable, self.raises = action, enable, raises
        self.message = ""

    async def elicit(self, message, schema):
        if self.raises:
            raise RuntimeError("client does not support elicitation")
        self.message = message

        class _Result:
            pass

        result = _Result()
        result.action = self.action
        data = _Result()
        data.enable = self.enable
        result.data = data
        return result


async def test_declining_writes_nothing(monkeypatch, tmp_path):
    monkeypatch.setattr(settings, "download_dir", None)
    from search_mcp.server import download

    ctx = _Ctx(action="decline", enable=False)
    with pytest.raises(PermissionError, match="cancelled"):
        await download("https://x.example/a.bin", ctx=ctx)
    assert downloads.is_enabled() is False


async def test_the_prompt_states_where_and_for_how_long(monkeypatch):
    monkeypatch.setattr(settings, "download_dir", None)
    monkeypatch.setattr(settings, "download_ttl_hours", 24)
    from search_mcp.server import download

    ctx = _Ctx(action="decline", enable=False)
    with pytest.raises(PermissionError):
        await download("https://x.example/a.bin", ctx=ctx)
    assert "24h" in ctx.message
    assert str(downloads.default_download_dir()) in ctx.message


async def test_client_without_elicitation_gets_an_actionable_error(monkeypatch):
    monkeypatch.setattr(settings, "download_dir", None)
    from search_mcp.server import download

    with pytest.raises(PermissionError, match="SEARCH_MCP_DOWNLOAD_DIR"):
        await download("https://x.example/a.bin", ctx=_Ctx(raises=True))


async def test_no_context_at_all_is_refused_not_assumed(monkeypatch):
    """Absent a way to ask, the answer is no."""
    monkeypatch.setattr(settings, "download_dir", None)
    from search_mcp.server import download

    with pytest.raises(PermissionError):
        await download("https://x.example/a.bin", ctx=None)


async def test_accepting_saves_the_file(monkeypatch, tmp_path):
    monkeypatch.setattr(settings, "download_dir", None)
    monkeypatch.setattr(downloads, "default_download_dir", lambda: tmp_path / "dl")

    from search_mcp import fetcher
    from search_mcp.server import download

    async def fake_fetch_bytes(url):
        return fetcher.FetchResult(
            url=url, title="a.bin", content="", method="asset", truncated=False,
            media_type="application/octet-stream", bytes_size=4,
            sha256="abc", data=b"data",
        )

    monkeypatch.setattr("search_mcp.server.fetch_bytes", fake_fetch_bytes)

    out = await download("https://x.example/a.bin", ctx=_Ctx(), format="json")

    def _on_disk(path: str) -> bytes:
        # Read outside the async frame: ASYNC240 rightly objects to blocking
        # pathlib calls inside a coroutine.
        return Path(path).read_bytes()

    assert _on_disk(out["saved_path"]) == b"data"
    assert out["expires_in_hours"] == settings.download_ttl_hours
