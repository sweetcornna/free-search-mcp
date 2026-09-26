"""`search-mcp-login codex | status | logout`, offline.

The browser flow itself is covered in test_oauth.py; here `oauth.login` is a
fake, and what is checked is what the command prints and returns.
"""
from __future__ import annotations

import base64
import json
import sys
import time

import pytest

from search_mcp import login, oauth


def _run(monkeypatch, *argv: str) -> int:
    monkeypatch.setattr(sys, "argv", ["search-mcp-login", *argv])
    with pytest.raises(SystemExit) as excinfo:
        login.main()
    return excinfo.value.code


def test_status_when_nothing_is_signed_in(monkeypatch, capsys):
    assert _run(monkeypatch, "status") == 0
    out = capsys.readouterr().out
    assert "codex" in out and "not signed in" in out and "search-mcp-login codex" in out


def test_a_sign_in_prints_the_link_and_who_signed_in(monkeypatch, capsys):
    async def fake_login(provider_id, *, open_browser, paste, on_url, **kwargs):
        assert (provider_id, open_browser, paste) == ("codex", False, True)
        on_url("https://auth.openai.com/oauth/authorize?redirect_uri="
               "http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback&state=s")
        cred = oauth.Credential(provider="codex", access_token="SECRET", refresh_token="r",
                                expires_at=time.time() + 3600, account_id="acct",
                                email="me@example.com", plan="pro")
        oauth.save(cred)
        return cred

    monkeypatch.setattr(oauth, "login", fake_login)
    assert _run(monkeypatch, "codex", "--no-browser") == 0
    out = capsys.readouterr().out
    assert "https://auth.openai.com/oauth/authorize" in out
    # The address to paste back over SSH is named exactly.
    assert "http://127.0.0.1:1455/auth/callback?code=" in out
    assert "signed in as me@example.com" in out and "plan: pro" in out
    assert 'engines=["codex"]' in out
    assert "SECRET" not in out


def test_a_failed_sign_in_exits_non_zero_with_the_reason(monkeypatch, capsys):
    async def fake_login(provider_id, **kwargs):
        raise oauth.OAuthError("the sign-in was not completed: access_denied")

    monkeypatch.setattr(oauth, "login", fake_login)
    assert _run(monkeypatch, "codex", "--no-browser") == 1
    assert "access_denied" in capsys.readouterr().err


def test_use_codex_cli_links_the_existing_sign_in(monkeypatch, capsys):
    path = oauth.codex_cli_auth_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    claims = {"exp": int(time.time()) + 3600}
    access = "h." + base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=") + ".s"
    path.write_text(json.dumps({"auth_mode": "chatgpt", "tokens": {
        "access_token": access, "id_token": "", "refresh_token": "r", "account_id": "acct"}}))
    assert _run(monkeypatch, "codex", "--use-codex-cli") == 0
    assert "linked to the Codex CLI sign-in (read-only)" in capsys.readouterr().out
    assert oauth.load("codex").source == "codex-cli"


def test_logout(monkeypatch, capsys):
    oauth.save(oauth.Credential(provider="codex", access_token="a", refresh_token="r"))
    assert _run(monkeypatch, "logout", "codex") == 0
    assert "Signed out of codex" in capsys.readouterr().out
    assert oauth.load("codex") is None
    assert _run(monkeypatch, "logout", "codex") == 0
    assert "was not signed in" in capsys.readouterr().out
