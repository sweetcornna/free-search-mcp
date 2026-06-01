"""Admin backend tests — offline, via Starlette's TestClient (no uvicorn).

Each test points SEARCH_MCP_CONFIG_DIR at a tmp dir so the real ~/.config is
never touched, and resets the keystore hot-reload cache between cases.
"""
from __future__ import annotations

import pytest
from starlette.testclient import TestClient

from search_mcp import keystore
from search_mcp.admin import app


@pytest.fixture(autouse=True)
def _tmp_config(tmp_path, monkeypatch):
    monkeypatch.setenv("SEARCH_MCP_CONFIG_DIR", str(tmp_path))
    keystore._reset_cache()
    yield
    keystore._reset_cache()


@pytest.fixture
def client():
    with TestClient(app) as c:
        yield c


def test_index_renders_each_provider(client):
    resp = client.get("/")
    assert resp.status_code == 200
    body = resp.text
    for provider in keystore.PROVIDERS:
        assert provider.label in body
        # At least one how_to step is present...
        assert any(step.split("http")[0].strip()[:20] in body for step in provider.how_to)
        # ...and the signup_url is linked.
        assert provider.signup_url in body


def test_index_never_echoes_saved_secret(client):
    # Save a key first, then assert the raw value never appears in the HTML.
    keystore.set_secrets({"serper_api_key": "TOPSECRETVALUE42"})
    keystore._reset_cache()
    body = client.get("/").text
    assert "TOPSECRETVALUE42" not in body
    # But the configured badge should now reflect it.
    assert "Configured" in body


def test_save_persists_and_status_reflects(client):
    resp = client.post("/api/save", json={"serper_api_key": "SECRET123"})
    assert resp.status_code == 200
    data = resp.json()
    assert data["ok"] is True
    assert data["status"]["serper"] is True

    keystore._reset_cache()
    assert keystore.get_secret("serper_api_key") == "SECRET123"

    status = client.get("/api/status").json()
    assert status["providers"]["serper"] is True


def test_blank_value_does_not_wipe_existing(client):
    keystore.set_secrets({"serper_api_key": "KEEPME"})
    keystore._reset_cache()

    resp = client.post("/api/save", json={"serper_api_key": ""})
    assert resp.status_code == 200
    assert resp.json()["ok"] is True

    keystore._reset_cache()
    # Blank means "leave unchanged" — the existing key must survive.
    assert keystore.get_secret("serper_api_key") == "KEEPME"


def test_clear_removes_key(client):
    keystore.set_secrets({"serper_api_key": "DELETEME"})
    keystore._reset_cache()
    assert keystore.get_secret("serper_api_key") == "DELETEME"

    resp = client.post("/api/clear", json={"field": "serper_api_key"})
    assert resp.status_code == 200
    assert resp.json()["ok"] is True

    keystore._reset_cache()
    assert keystore.get_secret("serper_api_key") is None
    assert "serper_api_key" not in keystore.all_secrets()


def test_status_endpoint_shape(client):
    providers = client.get("/api/status").json()["providers"]
    assert set(providers) == {p.id for p in keystore.PROVIDERS}
