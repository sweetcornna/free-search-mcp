"""The sign-in module behind the `codex` engine.

Offline: the token endpoint is an `httpx.MockTransport`, and the loopback
callback is exercised by connecting to it from the test itself.
"""
from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import os
import socket
import stat
import time
from urllib.parse import parse_qs, urlparse

import httpx
import pytest

from search_mcp import oauth

# pytest.ini sets `asyncio_mode = auto` so async tests are auto-marked.


def _jwt(claims: dict) -> str:
    def seg(data: dict) -> str:
        return base64.urlsafe_b64encode(json.dumps(data).encode()).rstrip(b"=").decode()

    return f"{seg({'alg': 'none'})}.{seg(claims)}.sig"


def _tokens(exp: float, *, refresh: str = "r1", email: str = "me@example.com") -> dict:
    return {
        "access_token": _jwt({"exp": int(exp)}),
        "refresh_token": refresh,
        "id_token": _jwt(
            {
                "email": email,
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "acct-1",
                    "chatgpt_plan_type": "plus",
                },
            }
        ),
    }


def _stored(**fields) -> oauth.Credential:
    base = {"provider": "codex", "access_token": "a", "refresh_token": "r",
            "expires_at": time.time() + 3600, "account_id": "acct-1"}
    cred = oauth.Credential(**{**base, **fields})
    oauth.save(cred)
    return cred


@pytest.fixture
def mock_http(monkeypatch):
    """Route oauth's HTTP through a handler the test supplies."""
    calls: list[httpx.Request] = []
    state: dict = {"handler": None}

    def factory(engine=None, *, timeout=30.0):
        def handle(request: httpx.Request) -> httpx.Response:
            calls.append(request)
            return state["handler"](request)

        return httpx.AsyncClient(transport=httpx.MockTransport(handle))

    monkeypatch.setattr(oauth, "http_client", factory)
    return state, calls


def _body(request: httpx.Request) -> dict[str, str]:
    raw = request.content.decode()
    try:
        return json.loads(raw)
    except ValueError:
        return {k: v[0] for k, v in parse_qs(raw).items()}


# --- the pieces --------------------------------------------------------------------


def test_pkce_challenge_is_the_s256_of_the_verifier():
    verifier, challenge = oauth.pkce_pair()
    assert 43 <= len(verifier) <= 128
    digest = hashlib.sha256(verifier.encode()).digest()
    assert challenge == base64.urlsafe_b64encode(digest).rstrip(b"=").decode()


def test_the_authorize_link_is_the_codex_cli_one():
    spec = oauth.CODEX
    link = urlparse(spec.authorize_link("CHALLENGE", "STATE", spec.redirect_uri(1455)))
    assert f"{link.scheme}://{link.netloc}{link.path}" == "https://auth.openai.com/oauth/authorize"
    query = {k: v[0] for k, v in parse_qs(link.query).items()}
    assert query["client_id"] == "app_EMoamEEZ73f0CkXaXp7hrann"
    assert query["redirect_uri"] == "http://127.0.0.1:1455/auth/callback"
    assert query["code_challenge"] == "CHALLENGE"
    assert query["code_challenge_method"] == "S256"
    assert query["state"] == "STATE"
    assert query["scope"] == "openid profile email offline_access"
    assert query["codex_cli_simplified_flow"] == "true"
    assert query["id_token_add_organizations"] == "true"
    assert "+" not in link.query  # spaces are %20, not form-style


def test_only_the_registered_redirect_ports_are_used():
    # Both are on the Codex client's redirect allow-list; no other port works.
    assert oauth.CODEX.redirect_ports == (1455, 1457)


def test_a_redirect_is_accepted_only_with_the_matching_state():
    assert oauth.read_redirect({"code": ["abc"], "state": ["s1"]}, "s1") == "abc"
    # ChatGPT may append an onboarding suffix to the state.
    assert oauth.read_redirect({"code": ["abc"], "state": ["s1-suffix"]}, "s1") == "abc"
    with pytest.raises(oauth.OAuthError, match="state mismatch"):
        oauth.read_redirect({"code": ["abc"], "state": ["other"]}, "s1")
    with pytest.raises(oauth.OAuthError, match="state mismatch"):
        oauth.read_redirect({"code": ["abc"], "state": [""]}, "")
    with pytest.raises(oauth.OAuthError, match="access_denied"):
        oauth.read_redirect({"error": ["access_denied"], "state": ["s1"]}, "s1")
    with pytest.raises(oauth.OAuthError, match="no authorization code"):
        oauth.read_redirect({"state": ["s1"]}, "s1")


def test_a_workspace_without_codex_is_named_as_such():
    params = {"error": ["access_denied"],
              "error_description": ["missing_codex_entitlement"], "state": ["s"]}
    with pytest.raises(oauth.OAuthError, match="does not include Codex"):
        oauth.read_redirect(params, "s")


def test_a_pasted_redirect_address_yields_its_code():
    pasted = "http://127.0.0.1:1455/auth/callback?code=xyz&state=s1\n"
    assert oauth.parse_pasted(pasted, "s1") == "xyz"
    assert oauth.parse_pasted("http://localhost:1455/auth/callback?code=xyz&state=s1", "s1") == "xyz"
    assert oauth.parse_pasted("?code=xyz&state=s1", "s1") == "xyz"
    with pytest.raises(oauth.OAuthError, match="code="):
        oauth.parse_pasted("just some text", "s1")


def test_jwt_claims_tolerates_garbage():
    assert oauth.jwt_claims("not-a-jwt") == {}
    assert oauth.jwt_claims(_jwt({"exp": 5})) == {"exp": 5}


def test_the_workspace_id_and_email_come_from_the_id_token_claims():
    cred = oauth.Credential(provider="codex", id_token=_jwt({
        "https://api.openai.com/profile": {"email": "p@example.com"},
        "organizations": [{"id": "org-first"}],
    }))
    oauth._apply_codex_claims(cred)
    # Fallbacks when the namespaced auth claim is missing.
    assert cred.email == "p@example.com" and cred.account_id == "org-first"


# --- the store -----------------------------------------------------------------------


def test_nothing_is_signed_in_by_default():
    assert oauth.load("codex") is None
    assert oauth.status("codex") == {
        "provider": "codex",
        "label": oauth.CODEX.label,
        "engine": "codex",
        "signed_in": False,
    }


def test_a_saved_credential_is_private_and_round_trips():
    _stored(email="me@example.com", plan="plus")
    path = oauth.credential_path("codex")
    if os.name == "posix":
        assert stat.S_IMODE(path.stat().st_mode) == 0o600
        assert stat.S_IMODE(path.parent.stat().st_mode) == 0o700
    loaded = oauth.load("codex")
    assert loaded is not None
    assert (loaded.access_token, loaded.refresh_token, loaded.account_id) == ("a", "r", "acct-1")
    public = oauth.status("codex")
    assert public["signed_in"] and public["email"] == "me@example.com"
    assert "access_token" not in public and "refresh_token" not in public


def test_status_never_contains_a_token():
    tokens = _tokens(time.time() + 3600, refresh="REFRESH-SECRET")
    cred = oauth.Credential(provider="codex", access_token=tokens["access_token"],
                            refresh_token=tokens["refresh_token"], id_token=tokens["id_token"])
    oauth._apply_codex_claims(cred)
    oauth.save(cred)
    dumped = json.dumps(oauth.status("codex"))
    assert tokens["access_token"] not in dumped
    assert "REFRESH-SECRET" not in dumped
    assert "me@example.com" in dumped and "plus" in dumped


def test_a_corrupt_file_reads_as_signed_out():
    path = oauth.credential_path("codex")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("{not json", encoding="utf-8")
    assert oauth.load("codex") is None
    assert oauth.is_signed_in("codex") is False


def test_sign_out_deletes_the_file():
    _stored()
    assert oauth.sign_out("codex") is True
    assert oauth.load("codex") is None
    assert oauth.sign_out("codex") is False


def test_an_unknown_provider_is_a_value_error():
    with pytest.raises(ValueError, match="use codex"):
        oauth.provider("antigravity")


def test_opt_in_engines_reports_a_sign_in():
    from search_mcp.keystore import opt_in_engines

    assert opt_in_engines()["codex"] is False
    _stored()
    assert opt_in_engines()["codex"] is True


# --- refresh ---------------------------------------------------------------------------


async def test_a_fresh_credential_is_returned_without_a_request(mock_http):
    _, calls = mock_http
    _stored()
    cred = await oauth.credential("codex")
    assert cred.access_token == "a"
    assert calls == []


async def test_an_expiring_token_is_refreshed_the_way_the_cli_does_it(mock_http):
    state, calls = mock_http
    new = _tokens(time.time() + 7200, refresh="r2")
    state["handler"] = lambda request: httpx.Response(200, json=new)
    _stored(access_token="old", refresh_token="r1", expires_at=time.time() + 10)

    cred = await oauth.credential("codex")

    assert len(calls) == 1
    sent = calls[0]
    assert str(sent.url) == "https://auth.openai.com/oauth/token"
    # A JSON body with exactly these three fields, as codex-rs sends it.
    assert sent.headers["content-type"].startswith("application/json")
    assert json.loads(sent.content) == {
        "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
        "grant_type": "refresh_token",
        "refresh_token": "r1",
    }
    assert cred.access_token == new["access_token"]
    # OpenAI rotates refresh tokens; losing the new one signs the server out.
    assert oauth.load("codex").refresh_token == "r2"
    assert cred.account_id == "acct-1" and cred.email == "me@example.com"
    assert cred.expires_at > time.time() + 7000


async def test_a_refresh_reply_without_a_refresh_token_keeps_the_old_one(mock_http):
    state, _ = mock_http
    new = _tokens(time.time() + 7200)
    del new["refresh_token"]
    state["handler"] = lambda request: httpx.Response(200, json=new)
    _stored(refresh_token="keep", expires_at=0)
    cred = await oauth.credential("codex")
    assert cred.refresh_token == "keep"


async def test_a_revoked_refresh_token_says_to_sign_in_again(mock_http):
    state, _ = mock_http
    state["handler"] = lambda request: httpx.Response(
        401, json={"error": {"code": "refresh_token_reused", "message": "already used"}}
    )
    _stored(expires_at=0)
    with pytest.raises(oauth.OAuthError, match="search-mcp-login codex"):
        await oauth.credential("codex")


async def test_an_unreachable_token_host_suggests_the_proxy(mock_http):
    state, _ = mock_http

    def refuse(request):
        raise httpx.ConnectError("refused", request=request)

    state["handler"] = refuse
    _stored(expires_at=0)
    with pytest.raises(oauth.OAuthError, match="SEARCH_MCP_PROXY"):
        await oauth.credential("codex")


async def test_concurrent_callers_share_one_refresh(mock_http):
    state, calls = mock_http
    state["handler"] = lambda request: httpx.Response(
        200, json=_tokens(time.time() + 7200, refresh="r2")
    )
    _stored(access_token="old", expires_at=0)
    creds = await asyncio.gather(*(oauth.credential("codex") for _ in range(5)))
    assert len(calls) == 1
    assert len({c.access_token for c in creds}) == 1


async def test_a_forced_refresh_runs_even_when_the_token_looks_fresh(mock_http):
    state, calls = mock_http
    state["handler"] = lambda request: httpx.Response(
        200, json=_tokens(time.time() + 7200, refresh="r2")
    )
    _stored()
    await oauth.credential("codex", force_refresh=True)
    assert len(calls) == 1


async def test_not_signed_in_is_its_own_error():
    with pytest.raises(oauth.NotSignedIn):
        await oauth.credential("codex")


# --- the Codex CLI link ------------------------------------------------------------------


def _write_codex_cli(exp: float, **extra) -> dict:
    tokens = _tokens(exp)
    path = oauth.codex_cli_auth_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({
        "auth_mode": "chatgpt",
        "OPENAI_API_KEY": None,
        "tokens": {**tokens, "account_id": "acct-1"},
        "last_refresh": "2026-09-20T00:00:00Z",
        **extra,
    }), encoding="utf-8")
    return tokens


async def test_the_codex_cli_link_reads_tokens_and_stores_none(mock_http):
    _, calls = mock_http
    tokens = _write_codex_cli(time.time() + 3600)
    oauth.link_codex_cli()
    stored = json.loads(oauth.credential_path("codex").read_text(encoding="utf-8"))
    assert stored["source"] == "codex-cli"
    assert "access_token" not in stored and "refresh_token" not in stored
    cred = await oauth.credential("codex")
    assert cred.access_token == tokens["access_token"]
    assert cred.account_id == "acct-1" and cred.plan == "plus"
    assert oauth.status("codex")["source"] == "codex-cli"
    assert calls == []


async def test_an_expired_codex_cli_link_is_never_refreshed_here(mock_http):
    _, calls = mock_http
    _write_codex_cli(time.time() - 60)
    oauth.link_codex_cli()
    before = oauth.codex_cli_auth_path().read_bytes()
    with pytest.raises(oauth.OAuthError, match="Run `codex` once"):
        await oauth.credential("codex")
    with pytest.raises(oauth.OAuthError, match="was rejected"):
        await oauth.credential("codex", force_refresh=True)
    assert calls == []
    assert oauth.codex_cli_auth_path().read_bytes() == before


def test_linking_without_a_codex_cli_sign_in_says_how_to_get_one():
    with pytest.raises(oauth.OAuthError, match="codex login"):
        oauth.link_codex_cli()


def test_an_api_key_codex_login_is_not_a_chatgpt_sign_in():
    _write_codex_cli(time.time() + 3600, auth_mode="apikey", OPENAI_API_KEY="sk-x")
    with pytest.raises(oauth.OAuthError, match="no ChatGPT sign-in"):
        oauth.link_codex_cli()


# --- the browser flow ------------------------------------------------------------------------


async def _get(port: int, target: str) -> tuple[str, str]:
    reader, writer = await asyncio.open_connection("127.0.0.1", port)
    writer.write(f"GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".encode())
    await writer.drain()
    raw = (await reader.read()).decode()
    writer.close()
    head, _, body = raw.partition("\r\n\r\n")
    return head.split("\r\n", 1)[0], body


def _free_ports(n: int) -> list[int]:
    socks = [socket.socket() for _ in range(n)]
    try:
        for sock in socks:
            sock.bind(("127.0.0.1", 0))
        return [sock.getsockname()[1] for sock in socks]
    finally:
        for sock in socks:
            sock.close()


@pytest.fixture
def ports(monkeypatch):
    """Point the Codex provider at free ports; 1455 may be taken on a dev box."""
    chosen = tuple(_free_ports(2))
    spec = oauth.OAuthProvider(**{**oauth.CODEX.__dict__, "redirect_ports": chosen})
    monkeypatch.setitem(oauth.PROVIDERS, "codex", spec)
    monkeypatch.setattr(oauth, "CODEX", spec)
    return chosen


def _browser(visit_path):
    """An `on_url` that plays the browser: it follows the link's redirect."""
    seen: list = []

    def on_url(url: str) -> None:
        seen.append(url)
        query = parse_qs(urlparse(url).query)
        redirect = urlparse(query["redirect_uri"][0])

        async def visit() -> None:
            await asyncio.sleep(0.05)
            seen.append(await _get(redirect.port, visit_path(query["state"][0])))

        asyncio.get_running_loop().create_task(visit())

    return on_url, seen


async def test_the_loopback_callback_completes_a_sign_in(mock_http, ports):
    state, calls = mock_http
    state["handler"] = lambda request: httpx.Response(200, json=_tokens(time.time() + 3600))
    on_url, seen = _browser(lambda s: f"/auth/callback?code=THE-CODE&state={s}")

    cred = await oauth.login("codex", open_browser=False, on_url=on_url, wait_seconds=5)
    await asyncio.sleep(0.05)

    assert cred.email == "me@example.com" and cred.account_id == "acct-1"
    link, (status, page) = seen[0], seen[1]
    assert status.endswith("200 OK") and "Signed in" in page
    sent = calls[0]
    assert sent.headers["content-type"] == "application/x-www-form-urlencoded"
    body = _body(sent)
    assert body["grant_type"] == "authorization_code"
    assert body["code"] == "THE-CODE"
    assert body["redirect_uri"] == f"http://127.0.0.1:{ports[0]}/auth/callback"
    challenge = parse_qs(urlparse(link).query)["code_challenge"][0]
    digest = hashlib.sha256(body["code_verifier"].encode()).digest()
    assert base64.urlsafe_b64encode(digest).rstrip(b"=").decode() == challenge
    assert oauth.is_signed_in("codex")


async def test_a_busy_first_port_falls_back_to_the_second(mock_http, ports):
    state, calls = mock_http
    state["handler"] = lambda request: httpx.Response(200, json=_tokens(time.time() + 3600))
    blocker = await asyncio.start_server(lambda r, w: None, "127.0.0.1", ports[0])
    try:
        on_url, _ = _browser(lambda s: f"/auth/callback?code=C&state={s}")
        await oauth.login("codex", open_browser=False, on_url=on_url, wait_seconds=5)
    finally:
        blocker.close()
    assert _body(calls[0])["redirect_uri"] == f"http://127.0.0.1:{ports[1]}/auth/callback"


async def test_a_forged_callback_is_refused_and_stores_nothing(mock_http, ports):
    _, calls = mock_http
    on_url, seen = _browser(lambda s: "/auth/callback?code=X&state=forged")
    with pytest.raises(oauth.OAuthError, match="state mismatch"):
        await oauth.login("codex", open_browser=False, on_url=on_url, wait_seconds=5)
    await asyncio.sleep(0.05)
    assert seen[1][0].endswith("400 Bad Request")
    assert calls == []
    assert not oauth.is_signed_in("codex")


async def test_a_sign_in_without_a_workspace_id_is_refused(mock_http, ports):
    state, _ = mock_http
    tokens = _tokens(time.time() + 3600)
    tokens["id_token"] = _jwt({"email": "me@example.com"})
    state["handler"] = lambda request: httpx.Response(200, json=tokens)
    on_url, _ = _browser(lambda s: f"/auth/callback?code=C&state={s}")
    with pytest.raises(oauth.OAuthError, match="workspace id"):
        await oauth.login("codex", open_browser=False, on_url=on_url, wait_seconds=5)
    assert not oauth.is_signed_in("codex")


async def test_busy_ports_are_reported_by_number(ports):
    blockers = [await asyncio.start_server(lambda r, w: None, "127.0.0.1", p) for p in ports]
    try:
        with pytest.raises(oauth.OAuthError, match=f"port {ports[0]} and {ports[1]}"):
            await oauth.login("codex", open_browser=False, wait_seconds=1)
    finally:
        for blocker in blockers:
            blocker.close()
