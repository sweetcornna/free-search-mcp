"""Keystore tests — env/file precedence, hot-reload, atomic write, providers.

Offline; each test points SEARCH_MCP_CONFIG_DIR at a tmp dir so the real
~/.config is never touched.
"""
from __future__ import annotations

import json
import os

import pytest

from search_mcp import keystore


@pytest.fixture(autouse=True)
def _tmp_config(tmp_path, monkeypatch):
    monkeypatch.setenv("SEARCH_MCP_CONFIG_DIR", str(tmp_path))
    keystore._reset_cache()
    yield
    keystore._reset_cache()


def test_missing_field_returns_none():
    assert keystore.get_secret("serper_api_key") is None


def test_set_and_get_roundtrip():
    keystore.set_secrets({"serper_api_key": "abc123"})
    assert keystore.get_secret("serper_api_key") == "abc123"
    # Persisted under a {"secrets": {...}} envelope.
    data = json.loads(keystore.config_path().read_text())
    assert data["secrets"]["serper_api_key"] == "abc123"


def test_file_written_0600(tmp_path):
    keystore.set_secrets({"tavily_api_key": "tvly-x"})
    mode = keystore.config_path().stat().st_mode & 0o777
    assert mode == 0o600, oct(mode)


def test_env_overrides_file(monkeypatch):
    keystore.set_secrets({"brave_api_key": "from-file"})
    monkeypatch.setenv("SEARCH_MCP_BRAVE_API_KEY", "from-env")
    assert keystore.get_secret("brave_api_key") == "from-env"


def test_empty_value_is_unset():
    keystore.set_secrets({"serper_api_key": "  "})
    assert keystore.get_secret("serper_api_key") is None


def test_empty_update_deletes_existing():
    keystore.set_secrets({"serper_api_key": "x"})
    keystore.set_secrets({"serper_api_key": ""})
    assert keystore.get_secret("serper_api_key") is None
    assert "serper_api_key" not in keystore.all_secrets()


def test_partial_update_preserves_other_fields():
    keystore.set_secrets({"serper_api_key": "a", "tavily_api_key": "b"})
    keystore.set_secrets({"serper_api_key": "a2"})  # only touch one
    assert keystore.get_secret("serper_api_key") == "a2"
    assert keystore.get_secret("tavily_api_key") == "b"


def test_hot_reload_picks_up_external_write():
    assert keystore.get_secret("serper_api_key") is None  # primes the cache
    # Simulate the admin process writing the file out-of-band.
    keystore.config_dir().mkdir(parents=True, exist_ok=True)
    keystore.config_path().write_text(json.dumps({"secrets": {"serper_api_key": "live"}}))
    # mtime changed -> next read must reflect it without a manual reset.
    assert keystore.get_secret("serper_api_key") == "live"


def test_corrupt_file_yields_empty_not_crash():
    keystore.config_dir().mkdir(parents=True, exist_ok=True)
    keystore.config_path().write_text("{ not json")
    assert keystore.get_secret("anything") is None
    assert keystore.all_secrets() == {}


def test_all_secrets_ignores_env(monkeypatch):
    keystore.set_secrets({"serper_api_key": "file-val"})
    monkeypatch.setenv("SEARCH_MCP_SERPER_API_KEY", "env-val")
    # all_secrets is the file view (for the admin UI), so it shows the file value.
    assert keystore.all_secrets().get("serper_api_key") == "file-val"


# --- provider catalogue ------------------------------------------------------


def test_providers_present_and_unique():
    ids = [p.id for p in keystore.PROVIDERS]
    assert {"brave_api", "serper", "tavily", "google_cse", "anysearch"} <= set(ids)
    assert len(ids) == len(set(ids))
    for p in keystore.PROVIDERS:
        assert p.signup_url.startswith("http")
        assert p.how_to and all(isinstance(s, str) for s in p.how_to)
        assert p.fields


def test_is_configured_requires_all_required_fields():
    # google_cse needs BOTH key and cx.
    assert keystore.is_configured("google_cse") is False
    keystore.set_secrets({"google_cse_api_key": "k"})
    assert keystore.is_configured("google_cse") is False  # cx still missing
    keystore.set_secrets({"google_cse_cx": "c"})
    assert keystore.is_configured("google_cse") is True


def test_optional_provider_always_configured():
    # anysearch has no required fields -> configured even with no key.
    assert keystore.is_configured("anysearch") is True


def test_provider_status_shape():
    st = keystore.provider_status()
    assert set(st) == {p.id for p in keystore.PROVIDERS}
    assert st["serper"] is False
    keystore.set_secrets({"serper_api_key": "x"})
    assert keystore.provider_status()["serper"] is True


def test_load_env_file_into_environ(tmp_path, monkeypatch):
    monkeypatch.delenv("SEARCH_MCP_SERPER_API_KEY", raising=False)
    monkeypatch.delenv("SEARCH_MCP_TAVILY_API_KEY", raising=False)
    envf = tmp_path / ".env"
    envf.write_text(
        "# a comment\n"
        "SEARCH_MCP_SERPER_API_KEY=from-dotenv\n"
        'SEARCH_MCP_TAVILY_API_KEY="quoted-val"\n'
        "NOT_OURS=ignored\n"
    )
    keystore.load_env_file_into_environ(envf)
    assert os.environ["SEARCH_MCP_SERPER_API_KEY"] == "from-dotenv"
    assert os.environ["SEARCH_MCP_TAVILY_API_KEY"] == "quoted-val"  # quotes stripped
    assert "NOT_OURS" not in os.environ  # only SEARCH_MCP_* loaded
    # get_secret now resolves it (env precedence).
    assert keystore.get_secret("serper_api_key") == "from-dotenv"


def test_load_env_file_does_not_override_real_env(tmp_path, monkeypatch):
    monkeypatch.setenv("SEARCH_MCP_SERPER_API_KEY", "real-env")
    envf = tmp_path / ".env"
    envf.write_text("SEARCH_MCP_SERPER_API_KEY=from-dotenv\n")
    keystore.load_env_file_into_environ(envf)
    assert os.environ["SEARCH_MCP_SERPER_API_KEY"] == "real-env"  # not overridden


def test_load_env_file_missing_is_noop(tmp_path):
    keystore.load_env_file_into_environ(tmp_path / "does-not-exist.env")  # no raise


def test_every_provider_engine_is_registered():
    """Each provider's engine must exist in ENGINES, or the admin 'Test' button
    and `engines=[...]` selection would hit 'Unknown engine'."""
    from search_mcp.engines import ENGINES, get_engine

    for p in keystore.PROVIDERS:
        assert p.engine in ENGINES, f"{p.id} -> {p.engine} not registered"
        assert get_engine(p.engine).name == p.engine
