"""The settings page's sign-in cards for the `codex` and `antigravity` engines.

Offline: `oauth.login` is replaced by a fake that plays the browser, so no
port is opened and no token endpoint is called.
"""
from __future__ import annotations

import asyncio
import time

import pytest
from starlette.testclient import TestClient

from search_mcp import admin, oauth
from search_mcp.config import settings


@pytest.fixture
def client():
    with TestClient(admin.app) as c:
        yield c


def _signed_in(email: str = "me@example.com") -> oauth.Credential:
    cred = oauth.Credential(provider="codex", access_token="SECRET-ACCESS",
                            refresh_token="SECRET-REFRESH",
                            expires_at=time.time() + 3600, account_id="acct-1",
                            email=email, plan="plus")
    oauth.save(cred)
    return cred


def _wait_for(client: TestClient, state: str) -> dict:
    for _ in range(100):
        progress = client.get("/api/oauth/codex/progress").json()
        if progress["state"] == state:
            return progress
        time.sleep(0.02)
    raise AssertionError(f"never reached {state!r}: {progress}")


def test_the_page_has_a_sign_in_card_and_never_a_token(client):
    _signed_in()
    page = client.get("/").text
    assert 'data-oauth="codex"' in page
    assert "Sign in / 登录" in page and "Sign out / 退出登录" in page
    assert "me@example.com" in page
    assert "search-mcp-login codex" in page
    assert "SECRET-ACCESS" not in page and "SECRET-REFRESH" not in page


def test_the_antigravity_card_carries_the_warning(client):
    page = client.get("/").text
    assert 'data-oauth="antigravity"' in page
    # Said on the card, before the button, in both languages.
    card = page[page.index('data-oauth="antigravity"'):]
    assert card.index("suspension") < card.index("Sign in / 登录")
    assert "封禁" in card


def test_the_antigravity_card_takes_a_client_and_never_shows_it(client):
    saved = client.post("/api/save", json={"antigravity_client_id": "SAVED-ID",
                                           "antigravity_client_secret": "SAVED-SECRET"})
    assert saved.json()["ok"]
    assert oauth.antigravity_client() == ("SAVED-ID", "SAVED-SECRET")
    page = client.get("/").text
    card = page[page.index('data-oauth="antigravity"'):]
    assert 'data-key="antigravity_client_id"' in card
    assert 'data-key="antigravity_client_secret"' in card
    assert "SAVED-ID" not in page and "SAVED-SECRET" not in page
    # A set value outranks the install, so the card says it is set.
    assert card.count("set, not shown") == 2
    # The Codex card has no client to set.
    codex = page[page.index('data-oauth="codex"'):page.index('data-oauth="antigravity"')]
    assert "data-key=" not in codex


def test_status_lists_the_provider(client):
    body = client.get("/api/oauth/status").json()
    assert body["codex"]["signed_in"] is False
    _signed_in()
    body = client.get("/api/oauth/status").json()
    assert body["codex"]["signed_in"] is True
    assert "access_token" not in body["codex"] and "refresh_token" not in body["codex"]


def test_start_hands_back_the_link_and_progress_reports_the_finish(client, monkeypatch):
    async def fake_login(provider_id, *, open_browser, paste, on_url, **kwargs):
        assert provider_id == "codex" and open_browser is False and paste is False
        on_url("https://auth.openai.com/oauth/authorize?client_id=x")
        await asyncio.sleep(0.05)
        return _signed_in()

    monkeypatch.setattr(admin.oauth, "login", fake_login)
    started = client.post("/api/oauth/codex/start").json()
    assert started == {"ok": True, "url": "https://auth.openai.com/oauth/authorize?client_id=x"}
    done = _wait_for(client, "done")
    assert done["status"]["signed_in"] is True
    assert done["status"]["email"] == "me@example.com"


def test_a_sign_in_that_fails_before_the_link_says_why(client, monkeypatch):
    async def fake_login(provider_id, **kwargs):
        raise oauth.OAuthError("port 1455 and 1457 on this machine are in use")

    monkeypatch.setattr(admin.oauth, "login", fake_login)
    started = client.post("/api/oauth/codex/start").json()
    assert started["ok"] is False and "1455" in started["error"]


def test_a_sign_in_that_fails_later_shows_up_in_progress(client, monkeypatch):
    async def fake_login(provider_id, *, on_url, **kwargs):
        on_url("https://auth.openai.com/oauth/authorize")
        await asyncio.sleep(0.02)
        raise oauth.OAuthError("the sign-in was not completed: access_denied")

    monkeypatch.setattr(admin.oauth, "login", fake_login)
    assert client.post("/api/oauth/codex/start").json()["ok"] is True
    failed = _wait_for(client, "error")
    assert "access_denied" in failed["error"]


def test_a_second_start_cancels_the_first(client, monkeypatch):
    started: list[int] = []

    async def fake_login(provider_id, *, on_url, **kwargs):
        started.append(1)
        on_url(f"https://auth.openai.com/oauth/authorize?n={len(started)}")
        await asyncio.sleep(30)

    monkeypatch.setattr(admin.oauth, "login", fake_login)
    first = client.post("/api/oauth/codex/start").json()
    second = client.post("/api/oauth/codex/start").json()
    assert first["url"].endswith("n=1") and second["url"].endswith("n=2")
    assert client.get("/api/oauth/codex/progress").json()["state"] == "waiting"


def test_sign_out_forgets_the_sign_in(client):
    _signed_in()
    body = client.post("/api/oauth/codex/logout").json()
    assert body["ok"] is True and body["status"]["signed_in"] is False
    assert oauth.load("codex") is None


def test_an_unknown_provider_is_a_404(client):
    assert client.post("/api/oauth/gemini-cli/start").status_code == 404
    assert client.get("/api/oauth/gemini-cli/progress").status_code == 404
    assert client.post("/api/oauth/gemini-cli/logout").status_code == 404


def test_the_test_button_reports_the_missing_sign_in(client, monkeypatch):
    # With automatic sign-in on, as shipped: the button must not start one.
    monkeypatch.setattr(settings, "codex_auto_signin", True)
    monkeypatch.setattr(oauth, "can_open_browser", lambda: True)

    async def sign_in(*args, **kwargs):
        raise AssertionError("the Test button started a sign-in")

    monkeypatch.setattr(oauth, "sign_in_on_first_use", sign_in)
    body = client.get("/api/test/codex").json()
    assert body["ok"] is False
    assert body["error"].startswith("codex not configured")
    body = client.get("/api/test/antigravity").json()
    assert body["ok"] is False
    assert body["error"].startswith("antigravity not configured")
